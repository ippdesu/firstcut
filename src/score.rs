//! 评分汇总（M1/M3）：像素分析 + AI 推理调度 + 加权总分

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use rayon::prelude::*;

use crate::ai;
use crate::ai::facedetect::YuNet;
use crate::ai::musiq::Musiq;
use crate::config::{MetricParams, ScoreWeights};
use crate::decode;
use crate::dedup;
use crate::metrics;
use crate::scan::{PhotoEntry, stem_of};

/// AI 推理引擎（MUSIQ + YuNet，进程内共享）
///
/// onnxruntime 的 run 需要 &mut self，用 Mutex 串行化调用；
/// ORT 内部自带线程级并行（intra-op），串行调用仍能吃到多核。
pub struct AiEngine {
    pub musiq: Option<Mutex<Musiq>>,
    pub yunet: Option<Mutex<YuNet>>,
}

impl AiEngine {
    /// 加载全部 AI 模型；模型缺失时返回错误（含下载指引）
    pub fn load() -> anyhow::Result<Self> {
        ai::ensure_models()?;
        Ok(AiEngine {
            musiq: Some(Mutex::new(Musiq::load()?)),
            yunet: Some(Mutex::new(YuNet::load()?)),
        })
    }

    /// 不加载 AI 模型（纯像素评分，用于快速预览）
    pub fn none() -> Self {
        AiEngine { musiq: None, yunet: None }
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

/// 对全部 JPG 并行做像素分析 + AI 推理（优先命中缓存），返回 stem -> 分析结果
pub fn analyze_jpgs(
    entries: &[PhotoEntry],
    ai: &AiEngine,
    cache: Option<&crate::cache::ScoreCache>,
) -> AnalysisOutcome {
    let params = MetricParams::default();
    let counter = AtomicUsize::new(0);
    let hits = AtomicUsize::new(0);
    let total = entries.iter().filter(|e| !e.is_raw).count().max(1);

    let (results, new_rows): (HashMap<String, AnalysisResult>, Vec<(String, u64, i64, AnalysisResult)>) = entries
        .par_iter()
        .filter(|e| !e.is_raw)
        .filter_map(|e| {
            let done = counter.fetch_add(1, Ordering::Relaxed) + 1;
            if done % 25 == 0 || done == total {
                eprintln!("[score] 进度 {}/{}", done, total);
            }
            // 缓存命中则跳过分析
            if let Some(c) = cache {
                if let Some((size, mtime)) = crate::cache::file_fingerprint(Path::new(&e.path)) {
                    if let Some(r) = c.get(&e.path, size, mtime) {
                        hits.fetch_add(1, Ordering::Relaxed);
                        return Some((stem_of(&e.filename), r, None));
                    }
                }
            }
            let result = analyze_one(e, &params, ai).ok().flatten()?;
            let row = crate::cache::file_fingerprint(Path::new(&e.path))
                .map(|(size, mtime)| (e.path.clone(), size, mtime, result));
            Some((stem_of(&e.filename), result, row))
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

/// 单张 JPG 的完整分析（像素指标 + MUSIQ 美学 + YuNet 人脸 + dHash）
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
    let exposure = metrics::exposure::exposure_score(&stats);

    let iso = e.iso.parse::<u32>().unwrap_or(100);
    let noise_metric = metrics::noise::dark_noise_metric(&img);
    let noise = metrics::noise::noise_score(noise_metric, iso, p.noise_k0);

    let dhash = dedup::dhash(&img.luma, img.width, img.height);

    // ---- AI 推理（失败视为该维度不可用，不阻断整批）----
    let mut composition = metrics::composition::composition_score(&[]);
    let mut aesthetic = 60.0;
    let mut faces = 0usize;

    if let Some(m) = &ai.musiq {
        aesthetic = m.lock().unwrap().score(&img.rgb, img.width, img.height).unwrap_or(60.0);
    }
    if let Some(y) = &ai.yunet {
        match y.lock().unwrap().detect(&img.rgb, img.width, img.height) {
            Ok(boxes) => {
                faces = boxes.len();
                composition = metrics::composition::composition_score(&boxes);
            }
            Err(err) => {
                use std::sync::Once;
                static LOGGED: Once = Once::new();
                LOGGED.call_once(|| eprintln!("[score] YuNet 检测失败（后续静默）: {err:#}"));
            }
        }
    }

    Ok(Some(AnalysisResult {
        scores: PixelScores {
            sharpness,
            exposure,
            noise,
            composition,
            aesthetic,
        },
        dhash,
        faces,
    }))
}
