//! 评分汇总（M1/M3）：像素分析 + AI 推理调度 + 加权总分

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use rayon::prelude::*;

use crate::ai;
use crate::ai::facedetect::Scrfd;
use crate::ai::iqa::ClipIqa;
use crate::ai::pose::{PoseDet, head_region};
use crate::ai::SessionPool;
use crate::config::{MetricParams, ScoreConfig, ScoreWeights};
use crate::decode;
use crate::dedup;
use crate::metrics;
use crate::scan::PhotoEntry;

/// AI 推理引擎（CLIPIQA + SCRFD + YOLOv8-pose 多 session 池，进程内共享）
///
/// onnxruntime 的 run 需要 &mut self，用 SessionPool 轮询分配实现并行；
/// 每 session 内部线程数 = 核数 / 池大小。
pub struct AiEngine {
    pub iqa: Option<SessionPool<ClipIqa>>,
    /// SCRFD 10g 人脸检测（M5 从 YuNet 替换）
    pub face: Option<SessionPool<Scrfd>>,
    pub pose: Option<SessionPool<PoseDet>>,
}

/// AI session 池大小（实测池化无收益：AI 非瓶颈且每 session 线程减半变慢，保持 1）
const AI_POOL_SIZE: usize = 1;

impl AiEngine {
    /// 加载全部 AI 模型；模型缺失时返回错误（含下载指引）
    pub fn load() -> anyhow::Result<Self> {
        ai::ensure_models()?;
        let intra = std::thread::available_parallelism()
            .map(|n| (n.get() / AI_POOL_SIZE).max(1))
            .unwrap_or(1);
        Ok(AiEngine {
            iqa: Some(SessionPool::new(
                (0..AI_POOL_SIZE).map(|_| ClipIqa::load(intra)).collect::<anyhow::Result<_>>()?,
            )),
            face: Some(SessionPool::new(
                (0..AI_POOL_SIZE).map(|_| Scrfd::load(intra)).collect::<anyhow::Result<_>>()?,
            )),
            pose: Some(SessionPool::new(
                (0..AI_POOL_SIZE).map(|_| PoseDet::load(intra)).collect::<anyhow::Result<_>>()?,
            )),
        })
    }

    /// 不加载 AI 模型（纯像素评分，用于快速预览）
    pub fn none() -> Self {
        AiEngine { iqa: None, face: None, pose: None }
    }
}

/// 单张照片的五维分数（0-100）
#[derive(Debug, Clone, Copy, Default)]
pub struct PixelScores {
    pub sharpness: f64,
    pub exposure: f64,
    pub noise: f64,
    pub composition: f64,
    pub aesthetic: f64,
}

/// 单张照片的完整分析结果：分数 + 感知哈希（连拍去重用）+ 人脸数
#[derive(Debug, Clone, Copy)]
pub struct AnalysisResult {
    pub scores: PixelScores,
    pub dhash: u64,
    pub faces: usize,
}

/// 加权总分（0-100，1 位小数）
pub fn total_score(s: &PixelScores, w: &ScoreWeights) -> f64 {
    let total = s.sharpness * w.sharpness
        + s.exposure * w.exposure
        + s.noise * w.noise
        + s.composition * w.composition
        + s.aesthetic * w.aesthetic;
    (total * 10.0).round() / 10.0
}

/// 分析结果 + 缓存统计
pub struct AnalysisOutcome {
    /// stem -> 分析结果
    pub results: HashMap<String, AnalysisResult>,
    /// 缓存命中数
    pub hits: usize,
    /// 新分析数
    pub misses: usize,
    /// 需要写入缓存的新行：(path, size, mtime, result)
    pub new_rows: Vec<(String, u64, i64, AnalysisResult)>,
}

/// 对全部 JPG 并行做像素分析 + AI 推理（优先命中缓存快照），返回分析结果
pub fn analyze_jpgs(
    entries: &[PhotoEntry],
    ai: &AiEngine,
    cache_rows: Option<&std::collections::HashMap<String, crate::cache::CacheRow>>,
    cfg: &ScoreConfig,
) -> AnalysisOutcome {
    let counter = AtomicUsize::new(0);
    let hits = AtomicUsize::new(0);
    let total = entries.iter().filter(|e| !e.is_raw).count().max(1);
    // 配置指纹参与缓存键：改了 --config 必须重算，否则会静默返回旧分数
    let cfg_hash = crate::config::config_fingerprint(cfg);

    let (results, new_rows): (HashMap<String, AnalysisResult>, Vec<(String, u64, i64, AnalysisResult)>) = entries
        .par_iter()
        .filter(|e| !e.is_raw)
        .filter_map(|e| {
            let done = counter.fetch_add(1, Ordering::Relaxed) + 1;
            if done % 25 == 0 || done == total {
                eprintln!("[score] 进度 {}/{}", done, total);
            }
            // 缓存命中则跳过分析（快照为纯数据，可跨线程共享）
            if let Some(rows) = cache_rows {
                if let Some((size, mtime)) = crate::cache::file_fingerprint(Path::new(&e.path)) {
                    if let Some(row) = rows.get(&e.path) {
                        if row.matches(size, mtime, crate::cache::CACHE_VERSION, cfg_hash) {
                            hits.fetch_add(1, Ordering::Relaxed);
                            return Some((e.pair_id().to_string(), row.result, None));
                        }
                    }
                }
            }
            let result = analyze_one(e, &cfg.metric, ai).ok().flatten()?;
            let row = crate::cache::file_fingerprint(Path::new(&e.path))
                .map(|(size, mtime)| (e.path.clone(), size, mtime, result));
            Some((e.pair_id().to_string(), result, row))
        })
        .fold(
            || (HashMap::new(), Vec::new()),
            |(mut m, mut v), (stem, result, row)| {
                m.insert(stem, result);
                if let Some(r) = row {
                    v.push(r);
                }
                (m, v)
            },
        )
        .reduce(
            || (HashMap::new(), Vec::new()),
            |(mut m1, mut v1), (m2, v2)| {
                m1.extend(m2);
                v1.extend(v2);
                (m1, v1)
            },
        );

    AnalysisOutcome {
        hits: hits.load(Ordering::Relaxed),
        misses: results.len().saturating_sub(hits.load(Ordering::Relaxed)),
        results,
        new_rows,
    }
}

/// 单张 JPG 的完整分析（像素指标 + CLIPIQA 美学 + SCRFD 人脸 + 姿态头部 + dHash）
///
/// 返回 None 表示解码失败（跳过）；Err 表示 AI 推理失败（整批中断的候选，当前跳过）
pub fn analyze_one(
    e: &PhotoEntry,
    p: &MetricParams,
    ai: &AiEngine,
) -> anyhow::Result<Option<AnalysisResult>> {
    let img = decode::load_analysis_image(Path::new(&e.path))?;
    let Some(img) = img else { return Ok(None) };

    // ---- 像素指标 ----
    let sharp_var = metrics::sharpness::tenengrad_variance(&img);
    let norm = metrics::sharpness::normalized_sharpness(sharp_var, img.luma_variance);
    let sharpness = metrics::sharpness::sharpness_score(norm, p.sharpness_k);

    let stats = metrics::exposure::exposure_stats(&img);

    let iso = e.iso.parse::<u32>().unwrap_or(100);
    let noise_metric = metrics::noise::dark_noise_metric(&img);
    let noise = metrics::noise::noise_score(noise_metric, iso, p.noise_k0);

    let dhash = dedup::dhash(&img.luma, img.width, img.height);

    // ---- AI 推理（失败视为该维度不可用，不阻断整批）----
    let mut composition = metrics::composition::composition_score(&[]);
    let mut aesthetic = 60.0;
    let mut faces = 0usize;
    // 清晰度：全局分 + 主体（人脸）区域分取高者
    let mut sharpness_region: Option<f64> = None;
    // 曝光：主体级人脸区域的平均亮度（None 表示无主体脸，回退全图）。
    // 取所有主体级人脸中的**最亮**一张：暗侧的脸往往是背光/遮挡，
    // 用最亮者避免个别阴影中的脸压低整张照片；
    // 且 exposure_score 内部还会把它夹在"全图 ~ 中灰"之间，保证只做单向修正。
    let mut subject_luma: Option<f64> = None;

    if let Some(m) = &ai.iqa {
        aesthetic = m.acquire().score(&img.rgb, img.width, img.height).unwrap_or(60.0);
    }
    if let Some(y) = &ai.face {
        match y.acquire().detect(&img.rgb, img.width, img.height) {
            Ok(boxes) => {
                faces = boxes.len();
                composition = metrics::composition::composition_score(&boxes);
                for f in boxes.iter().filter(|f| f.h >= 0.04) {
                    let mean = metrics::exposure::region_mean_luma(
                        &img.luma,
                        img.width,
                        img.height,
                        (f.x + f.w / 2.0) as f64,
                        (f.y + f.h / 2.0) as f64,
                        f.w as f64 / 2.0,
                        f.h as f64 / 2.0,
                    );
                    subject_luma = Some(subject_luma.map_or(mean, |m: f64| m.max(mean)));
                }
                // 最大主体级人脸（≥4% 高度）→ 主体区域锐度
                if let Some(biggest) = boxes
                    .iter()
                    .filter(|f| f.h >= 0.04)
                    .max_by(|a, b| {
                        (a.w * a.h).partial_cmp(&(b.w * b.h)).unwrap_or(std::cmp::Ordering::Equal)
                    })
                {
                    let cx = (biggest.x + biggest.w / 2.0) as f64;
                    let cy = (biggest.y + biggest.h / 2.0) as f64;
                    // 清晰度：主体区域（1.5× 框）reblur
                    let half_w = (biggest.w as f64 * 1.5).clamp(0.05, 0.5);
                    let half_h = (biggest.h as f64 * 1.5).clamp(0.05, 0.5);
                    let reblur = metrics::sharpness::reblur_p80_region(
                        &img.luma, img.width, img.height, cx, cy, half_w, half_h,
                    );
                    sharpness_region = Some(metrics::sharpness::region_sharpness_score(reblur));
                }
            }
            Err(err) => {
                use std::sync::Once;
                static LOGGED: Once = Once::new();
                LOGGED.call_once(|| eprintln!("[score] SCRFD 检测失败（后续静默）: {err:#}"));
            }
        }
    }

    // 没有主体级人脸区域分时，用姿态检测定位头部再评估（SCRFD 完全漏检、
    // 或检出的脸全部低于 4% 主体门槛时都要走这一步；
    // 只看 faces == 0 会漏掉"只有小脸"的情形，直接掉到中性地板）
    if sharpness_region.is_none() {
        if let Some(pp) = &ai.pose {
            match pp.acquire().detect(&img.rgb, img.width, img.height) {
                Ok(persons) => {
                    if let Some(biggest) = persons.iter().max_by(|a, b| {
                        (a.w * a.h).partial_cmp(&(b.w * b.h)).unwrap_or(std::cmp::Ordering::Equal)
                    }) {
                        if let Some((cx, cy, half_w, half_h)) = head_region(biggest) {
                            let reblur = metrics::sharpness::reblur_p80_region(
                                &img.luma, img.width, img.height, cx, cy, half_w, half_h,
                            );
                            sharpness_region =
                                Some(metrics::sharpness::region_sharpness_score(reblur));
                        }
                    }
                }
                Err(err) => {
                    use std::sync::Once;
                    static LOGGED: Once = Once::new();
                    LOGGED.call_once(|| eprintln!("[score] 姿态检测失败（后续静默）: {err:#}"));
                }
            }
        }
    }

    // 主体区域分与全局分**取高者**（不是"命中即用区域分"）：
    // 区域估计偶发偏低时不至于把整张照片拉下去；代价是跑焦但背景纹理繁杂的
    // 照片可能被全局分救回——真糊交给 gallery 人工复核。
    let sharpness_final = match sharpness_region {
        Some(region) if region > sharpness => region,
        // 无人脸/无主体线索时：全局指标对浅景深照片不可靠，
        // 给 50 分中性下限（用户确认其场景均为大光圈浅景深人像，
        // 宁可漏判真糊，不可误杀清晰照片；真糊由人工在 gallery 复核）
        _ => sharpness.max(50.0),
    };

    // 曝光：EV 容差带 + 主体感知（有主体脸时混入脸区域亮度，
    // 修正舞台黑幕布 / 白裙白背景两类"全图均值不代表主体"的误判）
    let exposure_curve = metrics::exposure::ExposureCurve {
        target: p.exposure_target,
        ev_full_lo: p.exposure_ev_full_lo,
        ev_full_hi: p.exposure_ev_full_hi,
        ev_lo: p.exposure_ev_lo,
        ev_hi: p.exposure_ev_hi,
        subject_blend: p.exposure_subject_blend,
    };
    let exposure = metrics::exposure::exposure_score(&stats, subject_luma, &exposure_curve);

    Ok(Some(AnalysisResult {
        scores: PixelScores {
            sharpness: sharpness_final,
            exposure,
            noise,
            composition,
            aesthetic,
        },
        dhash,
        faces,
    }))
}
