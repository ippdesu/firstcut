//! 复核快照构建：扫描目录 + 缓存 → 内存数据（M-UI1）
//!
//! 与 `score` 子命令共用同一套逻辑（analyze/bursts/ratings），
//! 保证 review 界面看到的星级、连拍信息与 CLI 输出逐张一致。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::cache::{self, ScoreCache};
use crate::config::ScoreConfig;
use crate::output;
use crate::scan;
use crate::score::{self, AnalysisResult};

/// 单张照片的 JSON 视图（相对路径是所有取图端点的 `p` 参数）
#[derive(Debug, Clone, Serialize)]
pub struct PhotoJson {
    /// 相对扫描根目录的路径（正斜杠）
    pub path: String,
    pub filename: String,
    pub ext: String,
    pub is_raw: bool,
    pub has_pair: bool,
    pub datetime: String,
    pub iso: String,
    pub f_number: String,
    pub shutter: String,
    pub focal: String,
    /// 五维分 + 总分；缓存未命中（未评分）为 None
    pub scores: Option<ScoresJson>,
    pub stars: Option<u8>,
    pub faces: usize,
    pub burst: Option<BurstJson>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScoresJson {
    pub sharpness: f64,
    pub exposure: f64,
    pub noise: f64,
    pub composition: f64,
    pub aesthetic: f64,
    pub total: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BurstJson {
    pub group: usize,
    pub size: usize,
    pub rank: usize,
    pub keep: bool,
    /// M9 姿态簇号（0 = 未启用/无簇）
    pub pose_cluster: usize,
}

/// 一次 review 会话的完整数据
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub root: String,
    pub photos: Vec<PhotoJson>,
}

/// 构建快照：扫描 + 打开缓存 + 按当前配置过滤命中 + 星级/连拍在线计算。
///
/// 缓存打不开（文件损坏/不存在）时降级为"全部未评分"，不阻塞浏览。
pub fn build_snapshot(root: &Path, cfg: &ScoreConfig, cache_path: &Path) -> anyhow::Result<Snapshot> {
    let entries = scan::scan_directory(root)?;
    let cfg_hash = crate::config::config_fingerprint(cfg);

    // 打开缓存并按「size+mtime+version+cfg_hash」过滤出本次可用的分析结果
    let mut analyzed: HashMap<String, AnalysisResult> = HashMap::new();
    let cache = match ScoreCache::open(cache_path, cfg_hash) {
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
            if row.matches(size, mtime, cache::CACHE_VERSION, cfg_hash) {
                analyzed.insert(e.pair_id().to_string(), row.result);
            }
        }
    }

    // 星级：与 score 相同——对「有分析的 JPG 配对键集合」按当前配置分档
    let rated: Vec<(String, f64)> = analyzed
        .iter()
        .map(|(k, r)| (k.clone(), score::total_score(&r.scores, &cfg.weights)))
        .collect();
    let ratings = output::xmp::assign_ratings(&rated, &cfg.metric);

    // 连拍：与 score 同一函数 + 同一参数合成（V2-2，保证两处逐张一致）
    let dedup_params = crate::config::effective_dedup(None, cfg);
    let burst_map = score::analyze_photo_bursts(&entries, &analyzed, &dedup_params, &cfg.weights);

    let mut photos: Vec<PhotoJson> = entries
        .iter()
        .map(|e| {
            let analyzed_result = analyzed.get(e.pair_id());
            let rel = rel_path(root, Path::new(&e.path));
            PhotoJson {
                path: rel,
                filename: e.filename.clone(),
                ext: e.extension.clone(),
                is_raw: e.is_raw,
                has_pair: e.has_pair,
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
                }),
                stars: ratings.get(e.pair_id()).copied(),
                faces: analyzed_result.map(|r| r.faces).unwrap_or(0),
                burst: burst_map.get(e.pair_id()).map(|i| BurstJson {
                    group: i.group,
                    size: i.size,
                    rank: i.rank,
                    keep: i.keep,
                    pose_cluster: i.pose_cluster,
                }),
            }
        })
        .collect();
    photos.sort_by(|a, b| a.path.cmp(&b.path));

    Ok(Snapshot {
        root: root.display().to_string(),
        photos,
    })
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
