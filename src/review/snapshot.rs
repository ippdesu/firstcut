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

/// 构建快照：扫描 + 打开缓存 + 按当前配置过滤命中 + 星级/连拍在线计算。
///
/// 缓存打不开（文件损坏/不存在）时降级为"全部未评分"，不阻塞浏览。
pub fn build_snapshot(root: &Path, cfg: &ScoreConfig, cache_path: &Path,
    manual_keep: &HashMap<String, bool>,
    scene_feedback: &HashMap<String, SceneFeedbackRecord>) -> anyhow::Result<Snapshot> {
    let entries = scan::scan_directory(root)?;
    let pixel_hash = crate::config::analysis_fingerprint(cfg, false);
    let ai_hash = crate::config::analysis_fingerprint(cfg, true);
    let models_available = crate::ai::ensure_models().is_ok();

    // 打开缓存并按「size+mtime+version+cfg_hash」过滤出本次可用的分析结果
    let mut analyzed: HashMap<String, AnalysisResult> = HashMap::new();
    let mut mode_by_pair: HashMap<String, bool> = HashMap::new();
    let cache = match ScoreCache::open(cache_path, pixel_hash) {
        Ok(c) => Some(c),
        Err(err) => {
            eprintln!("[review] 警告: 缓存不可用（{err:#}），全部照片将显示为未评分");
            None
        }
    };
    if let Some(c) = &cache {
        for e in entries.iter().filter(|e| !e.is_raw) {
            let Some((size, mtime)) = cache::file_fingerprint(Path::new(&e.path)) else {
                continue;
            };
            let Some(row) = c.rows().get(&e.path) else { continue };
            if row.matches(size, mtime, cache::CACHE_VERSION, pixel_hash)
                || (models_available
                    && row.matches(size, mtime, cache::CACHE_VERSION, ai_hash)) {
                analyzed.insert(e.pair_id().to_string(), row.result);
                mode_by_pair.insert(e.pair_id().to_string(), row.cfg_hash == ai_hash);
            }
        }
    }

    let selection = crate::selection::build(root, &entries, &analyzed, cfg, None)?;

    let mut photos: Vec<PhotoJson> = entries
        .iter()
        .map(|e| {
            let analyzed_result = analyzed.get(e.pair_id());
            let rel = rel_path(root, Path::new(&e.path));
            let ai_mode = mode_by_pair.get(e.pair_id()) == Some(&true);
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
                    keep: manual_keep.get(&rel).copied().unwrap_or(i.keep),
                    suggested_keep: i.keep,
                    manual_keep: manual_keep.get(&rel).copied(),
                    pose_cluster: i.pose_cluster,
                }),
            }
        })
        .collect();
    photos.sort_by(|a, b| a.path.cmp(&b.path));

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
    p.strip_prefix(root)
        .map(|r| r.display().to_string().replace('\\', "/"))
        .unwrap_or_else(|_| p.display().to_string().replace('\\', "/"))
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
}
