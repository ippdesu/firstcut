//! 用户复核决定。算法每次可以重算，人工星级与人工连拍保留覆盖统一独立保存在
//! `.firstcut/decisions.sqlite`（并兼容读取/单向迁移旧版 `.firstcut/burst-overrides.json`）。

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::dedup::BurstInfo;
use crate::scan::PhotoEntry;
use crate::storage::{Database, Folder};

const DECISIONS_DB: &str = "decisions.sqlite";
const LEGACY_BURST_JSON: &str = "burst-overrides.json";
const META_BURST_JSON_MIGRATED: &str = "burst_json_migrated";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RatingSource {
    Algorithm,
    Manual,
}

impl RatingSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Algorithm => "algorithm",
            Self::Manual => "manual",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectiveRating {
    pub stars: u8,
    pub source: RatingSource,
}

/// 统一后的单张连拍有效结果（算法建议 + 人工保留/舍弃覆盖）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveBurst {
    pub group: usize,
    pub size: usize,
    pub rank: usize,
    /// 当前是否保留（人工覆盖优先于算法建议）
    pub keep: bool,
    /// 算法给出的原始保留建议
    pub suggested_keep: bool,
    /// 人工决定；None 表示沿用算法建议
    pub manual_keep: Option<bool>,
    /// 姿态簇号（0 = 未启用/无簇）
    pub pose_cluster: usize,
}

fn relative_key(root: &Path, photo: &Path) -> Result<String> {
    let rel_buf = match photo.strip_prefix(root) {
        Ok(rel) => rel.to_path_buf(),
        Err(_) => crate::storage::relative_under_album(root, photo)?
            .with_context(|| format!("照片不在根目录内: {}", photo.display()))?,
    };
    let relative = rel_buf.to_string_lossy();
    #[cfg(windows)]
    let relative = relative.replace('\\', "/");
    #[cfg(not(windows))]
    let relative = relative.into_owned();
    Ok(relative)
}

fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS manual_rating (
            photo_path TEXT PRIMARY KEY,
            stars INTEGER NOT NULL CHECK(stars BETWEEN 1 AND 5)
        );
        CREATE TABLE IF NOT EXISTS manual_burst_keep (
            photo_path TEXT PRIMARY KEY,
            keep INTEGER NOT NULL CHECK(keep IN (0, 1))
        );
        CREATE TABLE IF NOT EXISTS decision_meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );",
    )
}

fn table_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1 LIMIT 1",
    )?;
    let mut rows = stmt.query([name])?;
    Ok(rows.next()?.is_some())
}

fn is_burst_json_migrated(conn: &Connection) -> rusqlite::Result<bool> {
    if !table_exists(conn, "decision_meta")? {
        return Ok(false);
    }
    let mut stmt = conn.prepare("SELECT value FROM decision_meta WHERE key = ?1")?;
    let mut rows = stmt.query([META_BURST_JSON_MIGRATED])?;
    Ok(rows
        .next()?
        .and_then(|row| row.get::<_, String>(0).ok())
        .is_some_and(|v| v == "1"))
}

fn load_legacy_burst_json(state: &Folder) -> Result<HashMap<String, bool>> {
    let bytes = state.file(LEGACY_BURST_JSON)?.read(32 * 1024 * 1024)?;
    match bytes {
        Some(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("解析旧版连拍覆盖文件失败: .firstcut/{LEGACY_BURST_JSON}")),
        None => Ok(HashMap::new()),
    }
}

pub fn load(root: &Path) -> Result<HashMap<String, u8>> {
    load_in(&root.join(".firstcut"))
}

pub fn load_in(state_dir: &Path) -> Result<HashMap<String, u8>> {
    let state = match Folder::open_or_create_explicit(state_dir, false) {
        Ok(dir) => dir,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(err) => return Err(err.into()),
    };
    let Some(db) = Database::open(state.file(DECISIONS_DB)?, false)? else {
        return Ok(HashMap::new());
    };
    if !table_exists(&db.conn, "manual_rating")? {
        return Ok(HashMap::new());
    }
    let mut stmt = db.conn.prepare("SELECT photo_path, stars FROM manual_rating")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u8>(1)?)))?;
    rows.collect::<rusqlite::Result<HashMap<_, _>>>()
        .map_err(Into::into)
}

pub fn set(root: &Path, jpg: &Path, stars: u8) -> Result<()> {
    set_in(&root.join(".firstcut"), root, jpg, stars)
}

pub fn set_in(state_dir: &Path, jpg_root: &Path, jpg: &Path, stars: u8) -> Result<()> {
    anyhow::ensure!((1..=5).contains(&stars), "星级必须在 1~5");
    let key = relative_key(jpg_root, jpg)?;
    let state = Folder::open_or_create_explicit(state_dir, true)?;
    let mut db = Database::open(state.file(DECISIONS_DB)?, true)?
        .context("人工评分数据库不可用")?;
    ensure_schema(&db.conn)?;
    db.conn.execute(
        "INSERT INTO manual_rating(photo_path, stars) VALUES (?1, ?2)
        ON CONFLICT(photo_path) DO UPDATE SET stars=excluded.stars",
        rusqlite::params![key, stars],
    )?;
    db.save()
}

/// 读取人工连拍保留覆盖（照片相对路径 → 是否保留）。
///
/// 优先读取 `decisions.sqlite` 中的 `manual_burst_keep`；若尚未完成旧版
/// `burst-overrides.json` 迁移，则以只读方式合并旧 JSON 条目（SQLite 优先）。
pub fn load_burst_overrides(root: &Path) -> Result<HashMap<String, bool>> {
    load_burst_overrides_in(&root.join(".firstcut"))
}

pub fn load_burst_overrides_in(state_dir: &Path) -> Result<HashMap<String, bool>> {
    let state = match Folder::open_or_create_explicit(state_dir, false) {
        Ok(dir) => dir,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(err) => return Err(err.into()),
    };
    let mut out = HashMap::new();
    let mut migrated = false;
    if let Some(db) = Database::open(state.file(DECISIONS_DB)?, false)? {
        migrated = is_burst_json_migrated(&db.conn)?;
        if table_exists(&db.conn, "manual_burst_keep")? {
            let mut stmt = db.conn.prepare("SELECT photo_path, keep FROM manual_burst_keep")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? != 0)))?;
            for row in rows {
                let (path, keep) = row?;
                out.insert(path, keep);
            }
        }
    }
    if !migrated {
        for (path, keep) in load_legacy_burst_json(&state)? {
            out.entry(path).or_insert(keep);
        }
    }
    Ok(out)
}

/// 设置或清除单张照片的人工连拍保留覆盖（`None` 表示恢复算法建议）。
///
/// 首次写入时在事务内自动把旧版 `burst-overrides.json` 导入 SQLite 并记录
/// 迁移标记，保证删除某条覆盖后不会被旧 JSON 复活，且不破坏原 JSON 文件。
pub fn set_burst_keep(root: &Path, jpg: &Path, keep: Option<bool>) -> Result<()> {
    set_burst_keep_in(&root.join(".firstcut"), root, jpg, keep)
}

pub fn set_burst_keep_in(
    state_dir: &Path,
    jpg_root: &Path,
    jpg: &Path,
    keep: Option<bool>,
) -> Result<()> {
    let key = relative_key(jpg_root, jpg)?;
    let state = Folder::open_or_create_explicit(state_dir, true)?;
    let mut db = Database::open(state.file(DECISIONS_DB)?, true)?
        .context("人工评分数据库不可用")?;
    ensure_schema(&db.conn)?;
    let legacy = if !is_burst_json_migrated(&db.conn)? {
        Some(load_legacy_burst_json(&state)?)
    } else {
        None
    };
    let tx = db.conn.transaction()?;
    if let Some(legacy_map) = legacy {
        for (legacy_path, legacy_keep) in legacy_map {
            tx.execute(
                "INSERT OR IGNORE INTO manual_burst_keep(photo_path, keep) VALUES (?1, ?2)",
                rusqlite::params![legacy_path, i64::from(legacy_keep)],
            )?;
        }
        tx.execute(
            "INSERT OR REPLACE INTO decision_meta(key, value) VALUES (?1, '1')",
            [META_BURST_JSON_MIGRATED],
        )?;
    }
    match keep {
        Some(val) => {
            tx.execute(
                "INSERT INTO manual_burst_keep(photo_path, keep) VALUES (?1, ?2)
                 ON CONFLICT(photo_path) DO UPDATE SET keep = excluded.keep",
                rusqlite::params![key, i64::from(val)],
            )?;
        }
        None => {
            tx.execute(
                "DELETE FROM manual_burst_keep WHERE photo_path = ?1",
                [key],
            )?;
        }
    }
    tx.commit()?;
    db.save()
}

/// JPG 的路径是人工决定的身份；配对键只负责把决定映射给同一张 ARW。
pub fn effective_ratings(
    root: &Path,
    entries: &[PhotoEntry],
    algorithm: &HashMap<String, u8>,
    manual: &HashMap<String, u8>,
) -> Result<HashMap<String, EffectiveRating>> {
    let mut out: HashMap<String, EffectiveRating> = algorithm
        .iter()
        .map(|(key, &stars)| {
            (
                key.clone(),
                EffectiveRating {
                    stars,
                    source: RatingSource::Algorithm,
                },
            )
        })
        .collect();
    for e in entries.iter().filter(|e| !e.is_raw) {
        let key = relative_key(root, Path::new(&e.path))?;
        if let Some(&stars) = manual.get(&key) {
            // 未分析的照片仍未评分；人工覆盖只替代已存在的算法建议。
            if let Some(value) = out.get_mut(e.pair_id()) {
                *value = EffectiveRating {
                    stars,
                    source: RatingSource::Manual,
                };
            }
        }
    }
    Ok(out)
}

/// 合并算法连拍结果与人工连拍保留覆盖，按 `pair_id` 输出 JPG/ARW 共享的有效连拍状态。
///
/// 仅对属于连拍组（`group > 0`）的照片应用人工覆盖；非连拍单张保持原样。
pub fn effective_bursts(
    root: &Path,
    entries: &[PhotoEntry],
    algorithm: &HashMap<String, BurstInfo>,
    manual: &HashMap<String, bool>,
) -> Result<HashMap<String, EffectiveBurst>> {
    let mut out: HashMap<String, EffectiveBurst> = algorithm
        .iter()
        .map(|(key, info)| {
            (
                key.clone(),
                EffectiveBurst {
                    group: info.group,
                    size: info.size,
                    rank: info.rank,
                    keep: info.keep,
                    suggested_keep: info.keep,
                    manual_keep: None,
                    pose_cluster: info.pose_cluster,
                },
            )
        })
        .collect();
    for e in entries.iter().filter(|e| !e.is_raw) {
        let key = relative_key(root, Path::new(&e.path))?;
        if let Some(&keep) = manual.get(&key) {
            if let Some(value) = out.get_mut(e.pair_id()) {
                if value.group > 0 {
                    value.keep = keep;
                    value.manual_keep = Some(keep);
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_survives_reload_and_maps_to_raw() {
        let dir = std::env::temp_dir().join(format!("firstcut_decisions_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let jpg = dir.join("a.jpg");
        set(&dir, &jpg, 1).unwrap();
        let manual = load(&dir).unwrap();
        let entries = vec![
            PhotoEntry {
                path: jpg.display().to_string(),
                pair_id: "pair".into(),
                ..Default::default()
            },
            PhotoEntry {
                path: dir.join("a.arw").display().to_string(),
                pair_id: "pair".into(),
                is_raw: true,
                ..Default::default()
            },
        ];
        let algorithm = HashMap::from([("pair".into(), 5)]);
        let effective = effective_ratings(&dir, &entries, &algorithm, &manual).unwrap();
        assert_eq!(
            effective["pair"],
            EffectiveRating {
                stars: 1,
                source: RatingSource::Manual
            }
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn burst_override_migrates_legacy_json_and_clear_does_not_resurrect() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join(".firstcut")).unwrap();
        std::fs::write(
            root.join(".firstcut/burst-overrides.json"),
            r#"{"JPG/a.jpg": false, "JPG/b.jpg": true}"#,
        )
        .unwrap();

        // 1) 未写入 SQLite 前，只读兼容读取旧 JSON
        let loaded = load_burst_overrides(root).unwrap();
        assert_eq!(loaded.get("JPG/a.jpg"), Some(&false));
        assert_eq!(loaded.get("JPG/b.jpg"), Some(&true));

        // 2) 清除 JPG/a.jpg 后，应把旧 JSON 一次性导入 SQLite 并删除 a，不被旧 JSON 复活
        let jpg_a = root.join("JPG").join("a.jpg");
        let jpg_c = root.join("JPG").join("c.jpg");
        set_burst_keep(root, &jpg_a, None).unwrap();
        let after_clear = load_burst_overrides(root).unwrap();
        assert!(!after_clear.contains_key("JPG/a.jpg"), "清除后不应被旧 JSON 复活");
        assert_eq!(after_clear.get("JPG/b.jpg"), Some(&true), "未动的旧 JSON 条目必须完整保留");

        // 3) 新增一条覆盖且此时读 manual_rating 也不报错
        set_burst_keep(root, &jpg_c, Some(false)).unwrap();
        assert!(load(root).unwrap().is_empty());
        let final_map = load_burst_overrides(root).unwrap();
        assert_eq!(final_map.get("JPG/b.jpg"), Some(&true));
        assert_eq!(final_map.get("JPG/c.jpg"), Some(&false));

        // 4) effective_bursts 将 JPG 决定映射到 pair_id（且忽略非连拍单张）
        let entries = vec![
            PhotoEntry {
                path: root.join("JPG").join("b.jpg").display().to_string(),
                pair_id: "pair_b".into(),
                ..Default::default()
            },
            PhotoEntry {
                path: root.join("RAW").join("b.ARW").display().to_string(),
                pair_id: "pair_b".into(),
                is_raw: true,
                ..Default::default()
            },
            PhotoEntry {
                path: jpg_c.display().to_string(),
                pair_id: "pair_c".into(),
                ..Default::default()
            },
        ];
        let algo = HashMap::from([
            ("pair_b".into(), BurstInfo { group: 1, size: 3, rank: 2, keep: false, pose_cluster: 1 }),
            ("pair_c".into(), BurstInfo { group: 0, size: 0, rank: 0, keep: false, pose_cluster: 0 }),
        ]);
        let eff = effective_bursts(root, &entries, &algo, &final_map).unwrap();
        assert!(eff["pair_b"].keep);
        assert!(!eff["pair_b"].suggested_keep);
        assert_eq!(eff["pair_b"].manual_keep, Some(true));
        assert_eq!(eff["pair_c"].manual_keep, None, "非连拍单张不应被人工连拍覆盖修改");
    }

    #[cfg(unix)]
    #[test]
    fn refuses_state_directory_symlink_outside_photo_root() {
        use std::os::unix::fs::symlink;

        let base = std::env::temp_dir().join(format!(
            "firstcut_decisions_symlink_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let root = base.join("photos");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, root.join(".firstcut")).unwrap();

        let result = set(&root, &root.join("photo.jpg"), 3);
        assert!(result.is_err());
        assert!(!outside.join("decisions.sqlite").exists());

        std::fs::remove_dir_all(base).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn refuses_database_file_symlink() {
        use std::os::unix::fs::symlink;

        let base = std::env::temp_dir().join(format!(
            "firstcut_decisions_file_symlink_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let root = base.join("photos");
        let outside = base.join("outside.sqlite");
        std::fs::create_dir_all(root.join(".firstcut")).unwrap();
        std::fs::write(&outside, b"leave this file alone").unwrap();
        symlink(&outside, root.join(".firstcut/decisions.sqlite")).unwrap();

        let result = set(&root, &root.join("photo.jpg"), 3);
        assert!(result.is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"leave this file alone");

        std::fs::remove_dir_all(base).unwrap();
    }
}
