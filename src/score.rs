//! 评分汇总（M1/M3）：像素分析 + AI 推理调度 + 加权总分

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use rayon::prelude::*;

use crate::ai;
use crate::ai::facedetect::Scrfd;
use crate::ai::iqa::ClipIqa;
use crate::ai::pose::{PoseDet, head_region};
use crate::ai::SessionPool;
use crate::config::{DedupParams, MetricParams, ScoreConfig, ScoreWeights};
use crate::decode;
use crate::dedup::{self, BurstInfo};
use crate::metrics;
use crate::scan::{self, PhotoEntry};

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
    /// 加载全部 AI 模型；模型缺失时返回错误（含下载指引）。
    /// `use_gpu`：实验性 DirectML（仅 `--features gpu` 构建生效）。
    pub fn load(use_gpu: bool) -> anyhow::Result<Self> {
        ai::ensure_models()?;
        let intra = std::thread::available_parallelism()
            .map(|n| (n.get() / AI_POOL_SIZE).max(1))
            .unwrap_or(1);
        Ok(AiEngine {
            iqa: Some(SessionPool::new(
                (0..AI_POOL_SIZE)
                    .map(|_| ClipIqa::load(intra, use_gpu))
                    .collect::<anyhow::Result<_>>()?,
            )),
            face: Some(SessionPool::new(
                (0..AI_POOL_SIZE)
                    .map(|_| Scrfd::load(intra, use_gpu))
                    .collect::<anyhow::Result<_>>()?,
            )),
            pose: Some(SessionPool::new(
                (0..AI_POOL_SIZE)
                    .map(|_| PoseDet::load(intra, use_gpu))
                    .collect::<anyhow::Result<_>>()?,
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
    /// M9 头部姿态描述子：最大主体级人脸的 5 个 SCRFD 关键点按人脸框归一化
    /// （对位置/尺度不变），连拍组内聚类"不同姿势"用。None = 无主体级人脸。
    pub pose_desc: Option<[f32; 10]>,
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

/// M9 头部姿态描述子：把 SCRFD 的 5 个关键点按人脸框归一化（10 维）。
///
/// 各分量 = (关键点坐标 − 人脸框左上) / 人脸框宽高，因此对照片中的
/// 位置与尺度不变——同一张脸转动头部/变化表情时关键点相对框的位置
/// 会移动，这正是连拍内区分"不同姿势"需要的几何量。
/// 框退化（宽或高为 0）时返回 None。
pub fn pose_descriptor(f: &crate::ai::facedetect::FaceBox) -> Option<[f32; 10]> {
    if f.w <= 1e-4 || f.h <= 1e-4 {
        return None;
    }
    let mut desc = [0.0f32; 10];
    for (j, p) in f.kps.iter().enumerate() {
        desc[j * 2] = (p.0 - f.x) / f.w;
        desc[j * 2 + 1] = (p.1 - f.y) / f.h;
    }
    Some(desc)
}

/// 对「已分析」的 JPG 做连拍分析，返回 配对键 -> BurstInfo。
///
/// `score` 子命令与 `review` 复核界面共用同一逻辑，
/// 保证两边的连拍分组/排名/保留逐张一致。
pub fn analyze_photo_bursts(
    entries: &[PhotoEntry],
    analyzed: &HashMap<String, AnalysisResult>,
    params: &DedupParams,
    weights: &ScoreWeights,
) -> HashMap<String, BurstInfo> {
    let jpg_idx: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| !e.is_raw && analyzed.contains_key(e.pair_id()))
        .map(|(i, _)| i)
        .collect();
    let mut map = HashMap::new();
    if jpg_idx.len() < 2 {
        return map;
    }
    let jpg_entries: Vec<PhotoEntry> = jpg_idx.iter().map(|&i| entries[i].clone()).collect();
    let hashes: Vec<u64> =
        jpg_idx.iter().map(|&i| analyzed[entries[i].pair_id()].dhash).collect();
    let descs: Vec<Option<[f32; 10]>> =
        jpg_idx.iter().map(|&i| analyzed[entries[i].pair_id()].pose_desc).collect();
    let scores: Vec<f64> = jpg_idx
        .iter()
        .map(|&i| total_score(&analyzed[entries[i].pair_id()].scores, weights))
        .collect();
    let infos = dedup::analyze_bursts(&jpg_entries, &hashes, &scores, &descs, params);
    for (&idx, info) in jpg_idx.iter().zip(infos.iter()) {
        map.insert(entries[idx].pair_id().to_string(), info.clone());
    }
    map
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

/// 对全部 JPG 并行做像素分析 + AI 推理（优先命中缓存快照），返回分析结果。
///
/// `on_progress(done, total)`：进度回调（每 25 张与最后一张各报一次）；
/// `score` 子命令用它打 stderr，`review` 用它更新跑批进度。
pub fn analyze_jpgs(
    entries: &[PhotoEntry],
    ai: &AiEngine,
    cache_rows: Option<&std::collections::HashMap<String, crate::cache::CacheRow>>,
    cfg: &ScoreConfig,
    on_progress: &(dyn Fn(usize, usize) + Sync),
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
                on_progress(done, total);
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
    // M9：最大主体级人脸（连拍姿态描述子的来源）
    let mut subject_face: Option<crate::ai::facedetect::FaceBox> = None;

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
                    subject_face = Some(*biggest);
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
        pose_desc: subject_face.as_ref().and_then(pose_descriptor),
    }))
}

// ---- M-UI2：评分流程抽库（score 子命令与 review 跑批共用同一实现） ----

/// 一次评分任务的输出选项
#[derive(Debug, Clone)]
pub struct ScoreJobOptions {
    pub output_csv: PathBuf,
    pub cache_path: PathBuf,
    pub no_cache: bool,
    /// 跳过 AI 推理（纯像素快速预览）
    pub no_ai: bool,
    pub xmp: bool,
    /// 实验性 DirectML（仅 `--features gpu` 构建生效）
    pub gpu: bool,
    /// CLI `-k` 显式覆盖（None = 用 `[dedup] keep_k` 配置）
    pub keep_override: Option<usize>,
}

/// 跑批事件（CLI 转成 stderr 行，review 转成进度状态）
pub enum ScoreEvent {
    Info(String),
    Progress { done: usize, total: usize },
    Finished { total_jpg: usize, hits: usize, misses: usize },
}

/// 跑批结果摘要
#[derive(Debug)]
pub struct ScoreJobSummary {
    pub total_jpg: usize,
    pub hits: usize,
    pub misses: usize,
    pub failed_jpgs: Vec<String>,
    pub unmapped_arw: usize,
    pub burst_members: usize,
    pub csv_path: PathBuf,
}

/// 完整评分流水线：扫描 → 分析+AI（缓存优先）→ 连拍去重 → 回填 → 星级 → XMP/CSV。
///
/// **不修改/删除任何照片文件**，只写 XMP 侧车（可选）、CSV、SQLite 缓存。
/// `score` 子命令与 `review` 的"重新评分"共用；同一输入输出逐字节一致。
pub fn run_score_job(
    dir: &Path,
    cfg: &ScoreConfig,
    opts: &ScoreJobOptions,
    on_event: &(dyn Fn(ScoreEvent) + Sync),
) -> anyhow::Result<ScoreJobSummary> {
    if opts.gpu && !cfg!(feature = "gpu") {
        anyhow::bail!("GPU 推理需要启用 DirectML 构建：cargo build --release --features gpu");
    }

    on_event(ScoreEvent::Info(format!(
        "发现文件中…（目录 {}）",
        dir.display()
    )));
    let mut entries = scan::scan_directory(dir)?;
    on_event(ScoreEvent::Info(format!("发现 {} 个文件（JPG/ARW）", entries.len())));

    // AI 引擎（加载失败降级为纯像素评分，不中断）
    let engine = if opts.no_ai {
        on_event(ScoreEvent::Info("跳过 AI 推理（--no-ai）".into()));
        AiEngine::none()
    } else {
        match AiEngine::load(opts.gpu) {
            Ok(e) => e,
            Err(err) => {
                on_event(ScoreEvent::Info(format!(
                    "AI 模型不可用，降级为纯像素评分（可加 --no-ai 关闭提示）: {err:#}"
                )));
                AiEngine::none()
            }
        }
    };

    // 增量缓存（打不开降级为不使用，不中断）
    let mut cache = if opts.no_cache {
        None
    } else {
        match crate::cache::ScoreCache::open(&opts.cache_path, crate::config::config_fingerprint(cfg))
        {
            Ok(c) => Some(c),
            Err(err) => {
                on_event(ScoreEvent::Info(format!("缓存不可用（本次不使用）: {err:#}")));
                None
            }
        }
    };

    // 分析（并行；进度经回调）
    let total_jpg = entries.iter().filter(|e| !e.is_raw).count();
    let cache_rows = cache.as_ref().map(|c| c.rows());
    let outcome = analyze_jpgs(&entries, &engine, cache_rows, cfg, &|done, total| {
        on_event(ScoreEvent::Progress { done, total });
    });
    on_event(ScoreEvent::Finished {
        total_jpg,
        hits: outcome.hits,
        misses: outcome.misses,
    });
    if let Some(c) = &mut cache {
        for (path, size, mtime, r) in &outcome.new_rows {
            let _ = c.put(path, *size, *mtime, r);
        }
        if let Err(err) = c.flush() {
            on_event(ScoreEvent::Info(format!("缓存写入失败: {err:#}")));
        }
    }

    // 连拍去重（score/review 同一函数 + 同一参数合成）
    let dedup_params = crate::config::effective_dedup(opts.keep_override, cfg);
    let analyzed_count = outcome.results.len();
    let burst_map =
        analyze_photo_bursts(&entries, &outcome.results, &dedup_params, &cfg.weights);
    let burst_members = burst_map.values().filter(|i| i.group != 0).count();
    on_event(ScoreEvent::Info(format!(
        "连拍去重: {analyzed_count} 张 JPG 中 {burst_members} 张属于连拍组"
    )));

    // 回填 + 异常报告
    let by_key: HashMap<String, AnalysisResult> = outcome.results;
    for e in entries.iter_mut() {
        let key = e.pair_id().to_string();
        if let Some(r) = by_key.get(&key) {
            apply_scores(e, &r.scores, r.faces, cfg);
        }
        if let Some(info) = burst_map.get(&key) {
            apply_burst(e, info);
        }
    }
    for e in entries.iter_mut() {
        e.analysis_ok = if e.total_score.is_empty() { "false" } else { "true" }.into();
    }
    let failed_jpgs: Vec<String> = entries
        .iter()
        .filter(|e| !e.is_raw && e.total_score.is_empty())
        .map(|e| e.path.clone())
        .collect();
    if !failed_jpgs.is_empty() {
        on_event(ScoreEvent::Info(format!(
            "警告: {} 张 JPG 解码/分析失败: {}",
            failed_jpgs.len(),
            failed_jpgs.iter().take(10).cloned().collect::<Vec<_>>().join(", ")
        )));
    }
    let unmapped_arw = entries.iter().filter(|e| e.is_raw && e.total_score.is_empty()).count();
    if unmapped_arw > 0 {
        on_event(ScoreEvent::Info(format!(
            "警告: {unmapped_arw} 张 ARW 无同名 JPG 可映射分数（analysis_ok=false）"
        )));
    }

    // 星级
    let rated: Vec<(String, f64)> = by_key
        .iter()
        .map(|(k, r)| (k.clone(), total_score(&r.scores, &cfg.weights)))
        .collect();
    let ratings = crate::output::xmp::assign_ratings(&rated, &cfg.metric);
    for e in entries.iter_mut() {
        if let Some(st) = ratings.get(e.pair_id()) {
            e.stars = st.to_string();
        }
    }

    // XMP 侧车（按侧车最终路径去重；跨目录配对两端各一份）
    if opts.xmp {
        let mut written = 0usize;
        let mut skipped = 0usize;
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for e in &entries {
            let key = e.pair_id();
            let Some(r) = by_key.get(key) else { continue };
            let stem = scan::stem_raw_of(&e.filename);
            let sidecar = Path::new(&e.path)
                .with_file_name(format!("{stem}.xmp"))
                .display()
                .to_string();
            if !seen.insert(sidecar) {
                continue;
            }
            let total = total_score(&r.scores, &cfg.weights);
            let rating = *ratings.get(key).unwrap_or(&3);
            match crate::output::xmp::write_sidecar(e, &r.scores, total, rating) {
                Ok(true) => written += 1,
                Ok(false) => skipped += 1,
                Err(err) => on_event(ScoreEvent::Info(format!("XMP 写入失败 {}: {err:#}", e.path))),
            }
        }
        on_event(ScoreEvent::Info(format!("XMP 侧车写入 {written} 个，跳过 {skipped} 个")));
    }

    crate::output::csv::write_csv(&opts.output_csv, &entries)?;
    on_event(ScoreEvent::Info(format!("CSV 已写出: {}", opts.output_csv.display())));

    Ok(ScoreJobSummary {
        total_jpg,
        hits: outcome.hits,
        misses: outcome.misses,
        failed_jpgs,
        unmapped_arw,
        burst_members,
        csv_path: opts.output_csv.clone(),
    })
}

/// 把分数写入 PhotoEntry 的 CSV 字段
fn apply_scores(e: &mut PhotoEntry, s: &PixelScores, faces: usize, cfg: &ScoreConfig) {
    e.sharpness_score = fmt(s.sharpness);
    e.exposure_score = fmt(s.exposure);
    e.noise_score = fmt(s.noise);
    e.composition_score = fmt(s.composition);
    e.aesthetic_score = fmt(s.aesthetic);
    e.total_score = fmt(total_score(s, &cfg.weights));
    e.faces = faces.to_string();
}

/// 把连拍信息写入 PhotoEntry 的 CSV 字段
fn apply_burst(e: &mut PhotoEntry, info: &BurstInfo) {
    e.burst_group = if info.group == 0 { "0".into() } else { info.group.to_string() };
    e.burst_size = if info.size == 0 { String::new() } else { info.size.to_string() };
    e.burst_rank = if info.rank == 0 { String::new() } else { info.rank.to_string() };
    e.burst_keep = if info.size == 0 { String::new() } else { info.keep.to_string() };
    e.burst_pose = if info.size == 0 { String::new() } else { info.pose_cluster.to_string() };
}

fn fmt(v: f64) -> String {
    format!("{:.1}", v)
}
