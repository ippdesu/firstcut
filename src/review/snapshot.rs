//! 复核快照构建：扫描目录 + 缓存 → 内存数据（M-UI1）
//!
//! 与 `score` 子命令共用同一套逻辑（analyze/bursts/ratings），
//! 保证 review 界面看到的星级、连拍信息与 CLI 输出逐张一致。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cache::{self, ScoreCache};
use crate::config::ScoreConfig;
use crate::scan;
use crate::score::{self, AnalysisResult};
use super::scene_feedback::SceneFeedbackRecord;

/// 单张照片的 JSON 视图（相对路径是所有取图端点的 `p` 参数）
#[derive(Debug, Clone, Serialize)]
pub struct PhotoJson {
    /// 相对扫描根目录的路径（正斜杠）
    pub path: String,
    pub filename: String,
    pub ext: String,
    pub is_raw: bool,
    pub has_pair: bool,
    #[serde(skip)]
    pub pair_id: String,
    pub datetime: String,
    pub iso: String,
    pub f_number: String,
    pub shutter: String,
    pub focal: String,
    /// 五维分 + 总分；缓存未命中（未评分）为 None
    pub scores: Option<ScoresJson>,
    pub stars: Option<u8>,
    pub rating_source: Option<String>,
    pub analysis_mode: Option<String>,
    pub faces: usize,
    pub burst: Option<BurstJson>,
    pub scene_hint: SceneHintJson,
    pub scene_feedback: Option<SceneFeedbackRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoresJson {
    pub sharpness: f64,
    pub exposure: f64,
    pub noise: f64,
    pub composition: f64,
    pub aesthetic: f64,
    pub total: f64,
    /// 建议曝光修正（EV；None = 容差带内不给建议）——改星重渲染侧车时要原样带回
    pub suggested_ev: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WeightsJson {
    pub sharpness: f64,
    pub exposure: f64,
    pub noise: f64,
    pub composition: f64,
    pub aesthetic: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SceneHintJson {
    pub id: &'static str,
    pub label: &'static str,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BurstJson {
    pub group: usize,
    pub size: usize,
    pub rank: usize,
    /// 当前是否保留（手动标记优先于自动建议）
    pub keep: bool,
    /// 自动算法给出的保留建议
    pub suggested_keep: bool,
    /// 手动决定；None 表示沿用自动建议
    pub manual_keep: Option<bool>,
    /// 姿态簇号（0 = 未启用/无簇）
    pub pose_cluster: usize,
}

/// 一次 review 会话的完整数据
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub root: String,
    pub weights: WeightsJson,
    pub photos: Vec<PhotoJson>,
}

impl Snapshot {
    pub fn empty(root: &Path, cfg: &ScoreConfig) -> Self {
        Self {
            root: root.display().to_string(),
            weights: WeightsJson {
                sharpness: cfg.weights.sharpness,
                exposure: cfg.weights.exposure,
                noise: cfg.weights.noise,
                composition: cfg.weights.composition,
                aesthetic: cfg.weights.aesthetic,
            },
            photos: Vec::new(),
        }
    }
}

/// 构建快照：扫描 + 打开缓存 + 按当前配置过滤命中 + 星级/连拍在线计算。
///
/// 缓存打不开（文件损坏/不存在）时降级为"全部未评分"，不阻塞浏览。
pub fn build_snapshot(root: &Path, cfg: &ScoreConfig, cache_path: &Path,
    manual_keep: &HashMap<String, bool>,
    scene_feedback: &HashMap<String, SceneFeedbackRecord>) -> anyhow::Result<Snapshot> {
    build_snapshot_with_options(root, cfg, cache_path, manual_keep, scene_feedback, true, false,
        &|_, _, _| {})
}

pub fn build_snapshot_with_options(root: &Path, cfg: &ScoreConfig, cache_path: &Path,
    manual_keep: &HashMap<String, bool>,
    scene_feedback: &HashMap<String, SceneFeedbackRecord>, include_raw: bool,
    skip_processed: bool,
    progress: &dyn Fn(&str, usize, usize)) -> anyhow::Result<Snapshot> {
    let entries = scan::scan_review_directory(
        root, &root.join(".firstcut").join("report.csv"), include_raw, skip_processed, progress)?;
    progress("匹配评分缓存", 0, entries.len());
    let pixel_hash = crate::config::analysis_fingerprint(cfg, false);
    let ai_hash = crate::config::analysis_fingerprint(cfg, true);
    let legacy_ai_hash = crate::ai::ensure_models()
        .ok()
        .map(|_| crate::config::legacy_ai_fingerprint(cfg));

    // 打开缓存并按「size+mtime+version+cfg_hash」过滤出本次可用的分析结果
    let mut analyzed: HashMap<String, AnalysisResult> = HashMap::new();
    let mut mode_by_pair: HashMap<String, bool> = HashMap::new();
    let cache = match ScoreCache::open_in(root, cache_path, pixel_hash, false) {
        Ok(c) => c,
        Err(err) => {
            super::quiet_log(format!("[review] 警告: 缓存不可用（{err:#}），全部照片将显示为未评分"));
            None
        }
    };
    if let Some(c) = &cache {
        for (index, e) in entries.iter().filter(|e| !e.is_raw).enumerate() {
            if index % 50 == 0 { progress("匹配评分缓存", index, entries.len()); }
            let full = Path::new(&e.path);
            let Some((size, mtime)) = crate::storage::FileRef::inside(root, full).ok()
                .and_then(|file| file.open_read().ok().flatten())
                .and_then(|file| file.metadata().ok())
                .and_then(|meta| cache::metadata_fingerprint(&meta)) else {
                continue;
            };
            // 优先采用有效的新版相对路径记录（避免重跑 --no-ai 后被旧版绝对路径 AI 记录反超）；
            // 仅当相对路径记录不存在或指纹不匹配时，再回退校验旧版绝对路径记录。
            let classify = |row: &cache::CacheRow| -> Option<bool> {
                if row.matches(size, mtime, cache::CACHE_VERSION, ai_hash)
                    || legacy_ai_hash
                        .is_some_and(|h| row.matches(size, mtime, cache::CACHE_VERSION, h))
                {
                    Some(true)
                } else if row.matches(size, mtime, cache::CACHE_VERSION, pixel_hash) {
                    Some(false)
                } else {
                    None
                }
            };
            if let Some((row, _)) =
                cache::find_matching(c.rows(), root, &e.path, |row| classify(row).is_some())
            {
                if let Some(is_ai) = classify(row) {
                    analyzed.insert(e.pair_id().to_string(), row.result);
                    mode_by_pair.insert(e.pair_id().to_string(), is_ai);
                }
            }
        }
    }

    progress("计算星级与连拍", 0, entries.len());
    let selection = crate::selection::build(root, &entries, &analyzed, cfg, None)?;

    // 手动连拍保留覆盖以 JPG 相对路径持久化，通过 pair_id 同步到配对 RAW。
    let mut manual_keep_by_pair: HashMap<&str, bool> = HashMap::new();
    for e in entries.iter().filter(|e| !e.is_raw) {
        let rel = rel_path(root, Path::new(&e.path));
        if let Some(&keep) = manual_keep.get(&rel) {
            manual_keep_by_pair.insert(e.pair_id(), keep);
        }
    }

    let mut photos: Vec<PhotoJson> = entries
        .iter()
        .map(|e| {
            let analyzed_result = analyzed.get(e.pair_id());
            let rel = rel_path(root, Path::new(&e.path));
            let ai_mode = mode_by_pair.get(e.pair_id()) == Some(&true);
            let manual_burst_keep = manual_keep_by_pair
                .get(e.pair_id())
                .copied()
                .or_else(|| manual_keep.get(&rel).copied());
            PhotoJson {
                path: rel.clone(),
                filename: e.filename.clone(),
                ext: e.extension.clone(),
                is_raw: e.is_raw,
                has_pair: e.has_pair,
                pair_id: e.pair_id.clone(),
                datetime: e.date_time_original.clone(),
                iso: e.iso.clone(),
                f_number: e.f_number.clone(),
                shutter: e.shutter_speed.clone(),
                focal: e.focal_length.clone(),
                scores: analyzed_result.map(|r| ScoresJson {
                    sharpness: r.scores.sharpness,
                    exposure: r.scores.exposure,
                    noise: r.scores.noise,
                    composition: r.scores.composition,
                    aesthetic: r.scores.aesthetic,
                    total: score::total_score(&r.scores, &cfg.weights),
                    suggested_ev: r.suggested_ev,
                }),
                stars: selection.ratings.get(e.pair_id()).map(|r| r.stars),
                rating_source: selection.ratings.get(e.pair_id()).map(|r| r.source.as_str().to_string()),
                analysis_mode: analyzed_result.map(|_| if ai_mode {
                    "ai".to_string()
                } else { "pixel".to_string() }),
                faces: analyzed_result.map(|r| r.faces).unwrap_or(0),
                scene_hint: scene_hint(analyzed_result, ai_mode),
                scene_feedback: scene_feedback.get(&rel).cloned(),
                burst: selection.bursts.get(e.pair_id()).map(|i| BurstJson {
                    group: i.group,
                    size: i.size,
                    rank: i.rank,
                    keep: manual_burst_keep.unwrap_or(i.keep),
                    suggested_keep: i.keep,
                    manual_keep: manual_burst_keep,
                    pose_cluster: i.pose_cluster,
                }),
            }
        })
        .collect();
    photos.sort_by(|a, b| a.path.cmp(&b.path));
    progress("准备页面数据", entries.len(), entries.len());

    Ok(Snapshot {
        root: root.display().to_string(),
        weights: WeightsJson {
            sharpness: cfg.weights.sharpness,
            exposure: cfg.weights.exposure,
            noise: cfg.weights.noise,
            composition: cfg.weights.composition,
            aesthetic: cfg.weights.aesthetic,
        },
        photos,
    })
}

fn scene_hint(analysis: Option<&AnalysisResult>, ai_mode: bool) -> SceneHintJson {
    match analysis {
        None => SceneHintJson { id: "unknown", label: "未识别", reason: "照片尚未评分，缺少场景线索。".into() },
        Some(_) if !ai_mode => SceneHintJson { id: "unknown", label: "未识别", reason: "当前只有像素分析，没有人物检测结果。".into() },
        Some(result) if result.faces > 0 => SceneHintJson {
            id: "portrait", label: "人像候选",
            reason: format!("检测到 {} 张人脸；舞台、运动等场景也可能有人物，需人工确认。", result.faces),
        },
        Some(_) => SceneHintJson {
            id: "unknown", label: "未识别",
            reason: "现有模型没有通用场景分类能力；未检测到人脸。".into(),
        },
    }
}


/// 绝对/相对路径 → 相对 root 的正斜杠路径（越界时退回完整路径展示）
fn rel_path(root: &Path, p: &Path) -> String {
    let path = p.strip_prefix(root).unwrap_or(p).to_string_lossy();
    #[cfg(windows)]
    { path.replace('\\', "/") }
    #[cfg(not(windows))]
    { path.into_owned() }
}

/// 从快照根目录解析 `p` 参数：越界（`..` 段、盘符/UNC/根锚定、根外）一律拒绝。
///
/// 按路径**段**判断而不是子串——文件名含 `..` 的合法照片（如 `a..b.jpg`）
/// 不能被误伤（V2-6）。canonicalize 后的包含判断是最后一道安全网。
/// 返回可用于读文件的真实路径；canonicalize 要求文件存在。
pub fn resolve_under(root: &Path, p: &str) -> Option<PathBuf> {
    let p = p.trim();
    if p.is_empty() {
        return None;
    }
    let rel = Path::new(p);
    if rel.has_root() {
        // 覆盖 /xxx、C:/xxx、\\server\share 等一切根锚定形态
        return None;
    }
    for comp in rel.components() {
        match comp {
            std::path::Component::ParentDir => return None,
            // Windows 盘符前缀（如 C:foo 不带根也指向别的卷的当前目录）
            std::path::Component::Prefix(_) => return None,
            _ => {}
        }
    }
    let full = root.join(rel);
    let canonical = full.canonicalize().ok()?;
    let root_canonical = root.canonicalize().ok()?;
    if canonical.starts_with(&root_canonical) { Some(canonical) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_rejects_traversal() {
        let dir = std::env::temp_dir().join("firstcut_review_test");
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("a.jpg"), b"x").unwrap();
        // 文件名含 .. 的合法照片不能被误伤（V2-6）
        std::fs::write(sub.join("a..b.jpg"), b"x").unwrap();

        // 存在的合法路径
        let ok = resolve_under(&dir, "sub/a.jpg").map(|p| p.display().to_string());
        assert!(ok.is_some(), "根内路径应可解析");
        let dots = resolve_under(&dir, "sub/a..b.jpg").map(|p| p.display().to_string());
        assert!(dots.is_some(), "文件名含 .. 是合法的，不应 403");

        // 越界、绝对路径、盘符、空串
        assert!(resolve_under(&dir, "../outside.jpg").is_none());
        assert!(resolve_under(&dir, "sub/../a.jpg").is_none(), ".. 段一律拒绝");
        assert!(resolve_under(&dir, "C:/Windows/win.ini").is_none(), "盘符绝对路径拒绝");
        assert!(resolve_under(&dir, "\\\\server\\share\\x.jpg").is_none(), "UNC 拒绝");
        assert!(resolve_under(&dir, "/etc/passwd").is_none(), "根锚定拒绝");
        assert!(resolve_under(&dir, "").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rel_path_uses_forward_slashes() {
        let root = Path::new("F:/photos");
        assert_eq!(rel_path(root, Path::new("F:/photos/JPG/a.JPG")), "JPG/a.JPG");
    }

    #[cfg(unix)]
    #[test]
    fn unix_filename_is_not_reinterpreted_as_another_path() {
        let root = Path::new("/album");
        assert_eq!(rel_path(root, &root.join(r"camera\a.jpg")), r"camera\a.jpg");
    }

    #[test]
    fn portable_ai_cache_and_manual_burst_override_propagate_to_paired_raw() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("album");
        std::fs::create_dir_all(root.join("JPG")).unwrap();
        std::fs::create_dir_all(root.join("RAW")).unwrap();
        let jpg_a = root.join("JPG/a.jpg");
        let jpg_b = root.join("JPG/b.jpg");
        let raw_a = root.join("RAW/a.ARW");
        let raw_b = root.join("RAW/b.ARW");
        image::RgbImage::from_pixel(32, 32, image::Rgb([100, 120, 140])).save(&jpg_a).unwrap();
        image::RgbImage::from_pixel(32, 32, image::Rgb([100, 120, 140])).save(&jpg_b).unwrap();
        std::fs::write(&raw_a, b"raw").unwrap();
        std::fs::write(&raw_b, b"raw").unwrap();
        // 写入同一连拍时间的 EXIF 复用报告，让两张图归入同一连拍组
        let report_entries = vec![
            scan::PhotoEntry {
                path: jpg_a.display().to_string(),
                filename: "a.jpg".into(),
                extension: "jpg".into(),
                date_time_original: "2026:09:27 12:00:00".into(),
                ..Default::default()
            },
            scan::PhotoEntry {
                path: jpg_b.display().to_string(),
                filename: "b.jpg".into(),
                extension: "jpg".into(),
                date_time_original: "2026:09:27 12:00:01".into(),
                ..Default::default()
            },
        ];
        crate::output::csv::write_csv_in(&root, &root.join(".firstcut/report.csv"), &report_entries, true).unwrap();

        // 以相对路径键 + AI 可移植指纹写入缓存
        let cfg = ScoreConfig::default();
        let ai_hash = crate::config::analysis_fingerprint(&cfg, true);
        let cache_path = root.join(".firstcut/cache.sqlite");
        let mut cache = ScoreCache::open_in(&root, &cache_path, ai_hash, true).unwrap().unwrap();
        for (p, total_s) in [(&jpg_a, 80.0), (&jpg_b, 70.0)] {
            let (size, mtime) = cache::file_fingerprint(p).unwrap();
            cache.put(
                &cache::cache_key(&root, p),
                size,
                mtime,
                &AnalysisResult {
                    scores: score::PixelScores {
                        sharpness: total_s,
                        exposure: total_s,
                        noise: total_s,
                        composition: total_s,
                        aesthetic: total_s,
                    },
                    dhash: 0,
                    faces: 1,
                    pose_desc: None,
                    suggested_ev: None,
                },
            ).unwrap();
        }
        cache.flush().unwrap();

        let overrides = HashMap::from([("JPG/a.jpg".to_string(), false)]);
        let snap = build_snapshot(&root, &cfg, &cache_path, &overrides, &HashMap::new()).unwrap();
        let photo_jpg = snap.photos.iter().find(|p| p.path == "JPG/a.jpg").unwrap();
        let photo_raw = snap.photos.iter().find(|p| p.path == "RAW/a.ARW").unwrap();
        assert_eq!(photo_jpg.analysis_mode.as_deref(), Some("ai"), "无本地模型目录也应可读已有可移植 AI 缓存");
        assert_eq!(photo_jpg.burst.as_ref().unwrap().manual_keep, Some(false));
        assert!(!photo_jpg.burst.as_ref().unwrap().keep);
        assert_eq!(photo_raw.burst.as_ref().unwrap().manual_keep, Some(false), "配对 RAW 应同步手动连拍覆盖");
        assert!(!photo_raw.burst.as_ref().unwrap().keep);
    }

    #[test]
    fn stale_relative_cache_row_does_not_shadow_valid_legacy_absolute_row() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("album");
        std::fs::create_dir_all(root.join("JPG")).unwrap();
        let jpg = root.join("JPG").join("a.jpg");
        image::RgbImage::from_pixel(32, 32, image::Rgb([100, 120, 140])).save(&jpg).unwrap();
        let (size, mtime) = cache::file_fingerprint(&jpg).unwrap();

        let cfg = ScoreConfig::default();
        let pixel_hash = crate::config::analysis_fingerprint(&cfg, false);
        let stale_hash = pixel_hash.wrapping_add(12345);
        let cache_path = root.join(".firstcut/cache.sqlite");

        // 先写入一条“旧版绝对路径键 + 匹配当前配置指纹”的有效记录
        let mut legacy_cache = ScoreCache::open_in(&root, &cache_path, pixel_hash, true).unwrap().unwrap();
        legacy_cache.put(
            &jpg.display().to_string(),
            size,
            mtime,
            &AnalysisResult {
                scores: score::PixelScores {
                    sharpness: 82.0,
                    exposure: 84.0,
                    noise: 86.0,
                    composition: 88.0,
                    aesthetic: 80.0,
                },
                dhash: 7,
                faces: 0,
                pose_desc: None,
                suggested_ev: None,
            },
        ).unwrap();
        legacy_cache.flush().unwrap();
        drop(legacy_cache);

        // 再写入一条“新版相对路径键 + 另一配置指纹”的不匹配记录
        let mut stale_cache = ScoreCache::open_in(&root, &cache_path, stale_hash, true).unwrap().unwrap();
        stale_cache.put(
            &cache::cache_key(&root, &jpg),
            size,
            mtime,
            &AnalysisResult {
                scores: score::PixelScores {
                    sharpness: 11.0,
                    exposure: 11.0,
                    noise: 11.0,
                    composition: 11.0,
                    aesthetic: 11.0,
                },
                dhash: 9,
                faces: 0,
                pose_desc: None,
                suggested_ev: None,
            },
        ).unwrap();
        stale_cache.flush().unwrap();
        drop(stale_cache);

        // 1) review 快照应跳过不匹配的相对路径记录，命中仍有效的旧版绝对路径记录
        let snap = build_snapshot(&root, &cfg, &cache_path, &HashMap::new(), &HashMap::new()).unwrap();
        let photo = snap.photos.iter().find(|p| p.path == "JPG/a.jpg").unwrap();
        let scores = photo.scores.as_ref().expect("不应被不匹配的相对路径记录挡成未评分");
        assert_eq!(scores.sharpness, 82.0);
        assert_eq!(photo.analysis_mode.as_deref(), Some("pixel"));

        // 2) score::analyze_jpgs 也应命中旧版绝对路径记录而非重算，并生成迁移行覆盖失效的相对路径行
        let entries = scan::scan_directory_with_options(&root, false, false).unwrap();
        let read_cache = ScoreCache::open_in(&root, &cache_path, pixel_hash, false).unwrap().unwrap();
        let outcome = score::analyze_jpgs(
            &root,
            &entries,
            &score::AiEngine::none(),
            Some(read_cache.rows()),
            &cfg,
            &|_, _| {},
        );
        assert_eq!(outcome.hits, 1, "应命中有效的旧版绝对路径缓存");
        assert_eq!(outcome.misses, 0, "不应触发重算");
        assert_eq!(outcome.new_rows.len(), 1, "应产出相对路径迁移行");
        assert_eq!(outcome.new_rows[0].0, "JPG/a.jpg");
        assert_eq!(outcome.new_rows[0].3.scores.sharpness, 82.0);
    }

    #[test]
    fn valid_relative_pixel_row_takes_precedence_over_legacy_absolute_ai_row() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("album");
        std::fs::create_dir_all(root.join("JPG")).unwrap();
        let jpg = root.join("JPG").join("a.jpg");
        image::RgbImage::from_pixel(32, 32, image::Rgb([100, 120, 140])).save(&jpg).unwrap();
        let (size, mtime) = cache::file_fingerprint(&jpg).unwrap();

        let cfg = ScoreConfig::default();
        let ai_hash = crate::config::analysis_fingerprint(&cfg, true);
        let pixel_hash = crate::config::analysis_fingerprint(&cfg, false);
        let cache_path = root.join(".firstcut/cache.sqlite");

        // 旧版绝对路径键保留着一条有效 AI 分数（91.0）
        let mut legacy_ai = ScoreCache::open_in(&root, &cache_path, ai_hash, true).unwrap().unwrap();
        legacy_ai.put(
            &jpg.display().to_string(),
            size,
            mtime,
            &AnalysisResult {
                scores: score::PixelScores {
                    sharpness: 91.0,
                    exposure: 91.0,
                    noise: 91.0,
                    composition: 91.0,
                    aesthetic: 91.0,
                },
                dhash: 1,
                faces: 1,
                pose_desc: None,
                suggested_ev: None,
            },
        ).unwrap();
        legacy_ai.flush().unwrap();
        drop(legacy_ai);

        // 新版重跑纯像素评分，写入相对路径键的有效纯像素分数（60.9）
        let mut fresh_pixel = ScoreCache::open_in(&root, &cache_path, pixel_hash, true).unwrap().unwrap();
        fresh_pixel.put(
            &cache::cache_key(&root, &jpg),
            size,
            mtime,
            &AnalysisResult {
                scores: score::PixelScores {
                    sharpness: 60.9,
                    exposure: 60.9,
                    noise: 60.9,
                    composition: 60.9,
                    aesthetic: 60.9,
                },
                dhash: 1,
                faces: 0,
                pose_desc: None,
                suggested_ev: None,
            },
        ).unwrap();
        fresh_pixel.flush().unwrap();
        drop(fresh_pixel);

        // 复核页必须显示最新写入的相对路径纯像素分数，而不是回退到旧绝对路径 AI 分数
        let snap = build_snapshot(&root, &cfg, &cache_path, &HashMap::new(), &HashMap::new()).unwrap();
        let photo = snap.photos.iter().find(|p| p.path == "JPG/a.jpg").unwrap();
        let scores = photo.scores.as_ref().unwrap();
        assert_eq!(scores.total, 60.9);
        assert_eq!(photo.analysis_mode.as_deref(), Some("pixel"));
    }
}
