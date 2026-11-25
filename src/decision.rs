//! 用户复核决定。算法每次可以重算，人工星级与人工连拍保留覆盖统一独立保存在
//! `.firstcut/decisions.sqlite`（并兼容读取/单向迁移旧版 `.firstcut/burst-overrides.json`）。

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::dedup::BurstInfo;
use crate::scan::PhotoEntry;
use crate::storage::{Database, FileRef, Folder};

const DECISIONS_DB: &str = "decisions.sqlite";
const LEGACY_BURST_JSON: &str = "burst-overrides.json";
const META_BURST_JSON_MIGRATED: &str = "burst_json_migrated";
const FINGERPRINT_SLICE_BYTES: usize = 64 * 1024;

/// 人工决定映射：相对路径 → `Vec<(photo_fp, value)>`。
/// `photo_fp` 为空串表示历史无指纹记录（仅当没有精确指纹匹配时才兼容回退）。
pub type DecisionMap<T> = HashMap<String, Vec<(String, T)>>;

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

fn photo_fingerprint(root: &Path, rel_key: &str) -> Result<String> {
    let mut file = FileRef::inside(root, &root.join(rel_key))
        .with_context(|| format!("无法安全打开照片文件以生成指纹: {rel_key}"))?
        .open_read()
        .with_context(|| format!("读取照片文件失败: {rel_key}"))?
        .with_context(|| format!("照片文件不存在或不可读，无法生成内容指纹: {rel_key}"))?;
    let len = file
        .metadata()
        .with_context(|| format!("读取照片元数据失败: {rel_key}"))?
        .len();
    let mut hasher = Sha256::new();
    hasher.update(len.to_le_bytes());
    let mut buf = [0u8; FINGERPRINT_SLICE_BYTES];
    let head_target =
        usize::try_from(len.min(FINGERPRINT_SLICE_BYTES as u64)).unwrap_or(FINGERPRINT_SLICE_BYTES);
    let mut read_head = 0usize;
    while read_head < head_target {
        let n = file
            .read(&mut buf[read_head..head_target])
            .with_context(|| format!("读取照片头部字节失败: {rel_key}"))?;
        if n == 0 {
            break;
        }
        read_head += n;
    }
    hasher.update(&buf[..read_head]);
    if len > FINGERPRINT_SLICE_BYTES as u64 {
        file.seek(SeekFrom::Start(len - FINGERPRINT_SLICE_BYTES as u64))
            .with_context(|| format!("定位照片尾部字节失败: {rel_key}"))?;
        let mut read_tail = 0usize;
        while read_tail < FINGERPRINT_SLICE_BYTES {
            let n = file
                .read(&mut buf[read_tail..])
                .with_context(|| format!("读取照片尾部字节失败: {rel_key}"))?;
            if n == 0 {
                break;
            }
            read_tail += n;
        }
        hasher.update(&buf[..read_tail]);
    }
    Ok(format!("v1:{len}:{:x}", hasher.finalize()))
}

fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&pragma)?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(1).is_ok_and(|name| name == column) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS manual_rating (
            photo_path TEXT NOT NULL,
            photo_fp TEXT NOT NULL DEFAULT '',
            stars INTEGER NOT NULL CHECK(stars BETWEEN 1 AND 5),
            PRIMARY KEY (photo_path, photo_fp)
        );
        CREATE TABLE IF NOT EXISTS manual_burst_keep (
            photo_path TEXT NOT NULL,
            photo_fp TEXT NOT NULL DEFAULT '',
            keep INTEGER NOT NULL CHECK(keep IN (0, 1)),
            PRIMARY KEY (photo_path, photo_fp)
        );
        CREATE TABLE IF NOT EXISTS decision_meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );",
    )?;
    if !has_column(conn, "manual_rating", "photo_fp")? {
        conn.execute_batch(
            "CREATE TABLE manual_rating_v2 (
                photo_path TEXT NOT NULL,
                photo_fp TEXT NOT NULL DEFAULT '',
                stars INTEGER NOT NULL CHECK(stars BETWEEN 1 AND 5),
                PRIMARY KEY (photo_path, photo_fp)
            );
            INSERT INTO manual_rating_v2 (photo_path, photo_fp, stars)
                SELECT photo_path, '', stars FROM manual_rating;
            DROP TABLE manual_rating;
            ALTER TABLE manual_rating_v2 RENAME TO manual_rating;",
        )?;
    }
    if !has_column(conn, "manual_burst_keep", "photo_fp")? {
        conn.execute_batch(
            "CREATE TABLE manual_burst_keep_v2 (
                photo_path TEXT NOT NULL,
                photo_fp TEXT NOT NULL DEFAULT '',
                keep INTEGER NOT NULL CHECK(keep IN (0, 1)),
                PRIMARY KEY (photo_path, photo_fp)
            );
            INSERT INTO manual_burst_keep_v2 (photo_path, photo_fp, keep)
                SELECT photo_path, '', keep FROM manual_burst_keep;
            DROP TABLE manual_burst_keep;
            ALTER TABLE manual_burst_keep_v2 RENAME TO manual_burst_keep;",
        )?;
    }
    Ok(())
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
    let map = load_ratings_in(state_dir)?;
    Ok(map
        .into_iter()
        .filter_map(|(k, v)| v.last().map(|(_, stars)| (k, *stars)))
        .collect())
}

pub fn load_ratings_in(state_dir: &Path) -> Result<DecisionMap<u8>> {
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
    let mut out: DecisionMap<u8> = HashMap::new();
    if has_column(&db.conn, "manual_rating", "photo_fp")? {
        let mut stmt = db
            .conn
            .prepare("SELECT photo_path, photo_fp, stars FROM manual_rating")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, u8>(2)?))
        })?;
        for row in rows {
            let (path, fp, stars) = row?;
            out.entry(path).or_default().push((fp, stars));
        }
    } else {
        let mut stmt = db.conn.prepare("SELECT photo_path, stars FROM manual_rating")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u8>(1)?)))?;
        for row in rows {
            let (path, stars) = row?;
            out.entry(path).or_default().push((String::new(), stars));
        }
    }
    Ok(out)
}

pub fn set(root: &Path, jpg: &Path, stars: u8) -> Result<()> {
    set_in(&root.join(".firstcut"), root, jpg, stars)
}

pub fn set_in(state_dir: &Path, jpg_root: &Path, jpg: &Path, stars: u8) -> Result<()> {
    anyhow::ensure!((1..=5).contains(&stars), "星级必须在 1~5");
    let key = relative_key(jpg_root, jpg)?;
    let fp = photo_fingerprint(jpg_root, &key)?;
    let state = Folder::open_or_create_explicit(state_dir, true)?;
    let mut db = Database::open(state.file(DECISIONS_DB)?, true)?
        .context("人工评分数据库不可用")?;
    ensure_schema(&db.conn)?;
    let tx = db.conn.transaction()?;
    tx.execute(
        "INSERT INTO manual_rating(photo_path, photo_fp, stars) VALUES (?1, ?2, ?3)
        ON CONFLICT(photo_path, photo_fp) DO UPDATE SET stars=excluded.stars",
        rusqlite::params![key, fp, stars],
    )?;
    if !fp.is_empty() {
        tx.execute(
            "DELETE FROM manual_rating WHERE photo_path = ?1 AND photo_fp = ''",
            [&key],
        )?;
    }
    tx.commit()?;
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
    let map = load_bursts_in(state_dir)?;
    Ok(map
        .into_iter()
        .filter_map(|(k, v)| v.last().map(|(_, keep)| (k, *keep)))
        .collect())
}

pub fn load_bursts_in(state_dir: &Path) -> Result<DecisionMap<bool>> {
    let state = match Folder::open_or_create_explicit(state_dir, false) {
        Ok(dir) => dir,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(err) => return Err(err.into()),
    };
    let mut out: DecisionMap<bool> = HashMap::new();
    let mut migrated = false;
    if let Some(db) = Database::open(state.file(DECISIONS_DB)?, false)? {
        migrated = is_burst_json_migrated(&db.conn)?;
        if table_exists(&db.conn, "manual_burst_keep")? {
            if has_column(&db.conn, "manual_burst_keep", "photo_fp")? {
                let mut stmt = db
                    .conn
                    .prepare("SELECT photo_path, photo_fp, keep FROM manual_burst_keep")?;
                let rows = stmt.query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)? != 0,
                    ))
                })?;
                for row in rows {
                    let (path, fp, keep) = row?;
                    out.entry(path).or_default().push((fp, keep));
                }
            } else {
                let mut stmt = db.conn.prepare("SELECT photo_path, keep FROM manual_burst_keep")?;
                let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? != 0)))?;
                for row in rows {
                    let (path, keep) = row?;
                    out.entry(path).or_default().push((String::new(), keep));
                }
            }
        }
    }
    if !migrated {
        for (path, keep) in load_legacy_burst_json(&state)? {
            let entries = out.entry(path).or_default();
            if entries.is_empty() {
                entries.push((String::new(), keep));
            }
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
    let fp = photo_fingerprint(jpg_root, &key)?;
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
            let legacy_fp = photo_fingerprint(jpg_root, &legacy_path).unwrap_or_default();
            tx.execute(
                "INSERT OR IGNORE INTO manual_burst_keep(photo_path, photo_fp, keep)
                 SELECT ?1, ?2, ?3
                 WHERE NOT EXISTS (
                     SELECT 1 FROM manual_burst_keep WHERE photo_path = ?1 AND photo_fp = ''
                 )",
                rusqlite::params![legacy_path, legacy_fp, i64::from(legacy_keep)],
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
                "INSERT INTO manual_burst_keep(photo_path, photo_fp, keep) VALUES (?1, ?2, ?3)
                 ON CONFLICT(photo_path, photo_fp) DO UPDATE SET keep = excluded.keep",
                rusqlite::params![key, fp, i64::from(val)],
            )?;
            if !fp.is_empty() {
                tx.execute(
                    "DELETE FROM manual_burst_keep WHERE photo_path = ?1 AND photo_fp = ''",
                    [&key],
                )?;
            }
        }
        None => {
            tx.execute(
                "DELETE FROM manual_burst_keep WHERE photo_path = ?1 AND photo_fp = ?2",
                rusqlite::params![key, fp],
            )?;
            if !fp.is_empty() {
                tx.execute(
                    "DELETE FROM manual_burst_keep WHERE photo_path = ?1 AND photo_fp = ''",
                    [&key],
                )?;
            }
        }
    }
    tx.commit()?;
    db.save()
}

/// 从原相册导入旧版 `decisions.sqlite` / `burst-overrides.json` 后，按原相册照片补齐内容指纹；
/// 无法读取照片的旧记录保留在数据库中（`photo_fp = ''`），但绝不在后续匹配中自动应用。
pub fn backfill_fingerprints_in(state_dir: &Path, jpg_root: &Path) -> Result<()> {
    backfill_imported_decisions_in(state_dir, jpg_root, true, true)
}

/// 仅对本次从当前相册新导入的文件执行指纹补齐/迁移（避免导入新卡的 `burst-overrides.json` 时
/// 意外把旧卡遗留的空指纹记录绑定到新卡同名照片），且严格先迁移 SQLite 再导入 JSON 以维持 SQLite 优先。
pub fn backfill_imported_decisions_in(
    state_dir: &Path,
    jpg_root: &Path,
    backfill_db_rows: bool,
    import_burst_json: bool,
) -> Result<()> {
    if !backfill_db_rows && !import_burst_json {
        return Ok(());
    }
    let state = match Folder::open_or_create_explicit(state_dir, false) {
        Ok(dir) => dir,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    let has_db = state.file(DECISIONS_DB)?.open_read()?.is_some();
    let has_legacy_json = state.file(LEGACY_BURST_JSON)?.open_read()?.is_some();
    if !(backfill_db_rows && has_db) && !(import_burst_json && has_legacy_json) {
        return Ok(());
    }
    let mut db = Database::open(state.file(DECISIONS_DB)?, true)?
        .context("人工评分数据库不可用")?;
    let upgraded_schema = !table_exists(&db.conn, "manual_rating")?
        || !has_column(&db.conn, "manual_rating", "photo_fp")?
        || !table_exists(&db.conn, "manual_burst_keep")?
        || !has_column(&db.conn, "manual_burst_keep", "photo_fp")?;
    ensure_schema(&db.conn)?;
    let mut changed = upgraded_schema;

    // Step A: 先为本次导入的 SQLite 空指纹记录补齐指纹，确保同照片在后续 JSON 导入时已占位（维持 SQLite 优先）
    if backfill_db_rows && has_db {
        let rating_rows: Vec<(String, u8)> = {
            let mut stmt = db
                .conn
                .prepare("SELECT photo_path, stars FROM manual_rating WHERE photo_fp = ''")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u8>(1)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let burst_rows: Vec<(String, i64)> = {
            let mut stmt = db
                .conn
                .prepare("SELECT photo_path, keep FROM manual_burst_keep WHERE photo_fp = ''")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };

        if !rating_rows.is_empty() || !burst_rows.is_empty() {
            let tx = db.conn.transaction()?;
            for (path, stars) in rating_rows {
                if let Ok(fp) = photo_fingerprint(jpg_root, &path) {
                    tx.execute(
                        "INSERT INTO manual_rating(photo_path, photo_fp, stars) VALUES (?1, ?2, ?3)
                         ON CONFLICT(photo_path, photo_fp) DO NOTHING",
                        rusqlite::params![&path, &fp, stars],
                    )?;
                    tx.execute(
                        "DELETE FROM manual_rating WHERE photo_path = ?1 AND photo_fp = ''",
                        [&path],
                    )?;
                    changed = true;
                }
            }
            for (path, keep) in burst_rows {
                if let Ok(fp) = photo_fingerprint(jpg_root, &path) {
                    tx.execute(
                        "INSERT INTO manual_burst_keep(photo_path, photo_fp, keep) VALUES (?1, ?2, ?3)
                         ON CONFLICT(photo_path, photo_fp) DO NOTHING",
                        rusqlite::params![&path, &fp, keep],
                    )?;
                    tx.execute(
                        "DELETE FROM manual_burst_keep WHERE photo_path = ?1 AND photo_fp = ''",
                        [&path],
                    )?;
                    changed = true;
                }
            }
            tx.commit()?;
        }
    }

    // Step B: 再导入本次的 burst-overrides.json（跳过 SQLite 已存在同指纹或空指纹的记录）
    if import_burst_json && has_legacy_json && !is_burst_json_migrated(&db.conn)? {
        let legacy_map = load_legacy_burst_json(&state)?;
        let tx = db.conn.transaction()?;
        for (legacy_path, legacy_keep) in legacy_map {
            let fp = photo_fingerprint(jpg_root, &legacy_path).unwrap_or_default();
            tx.execute(
                "INSERT OR IGNORE INTO manual_burst_keep(photo_path, photo_fp, keep)
                 SELECT ?1, ?2, ?3
                 WHERE NOT EXISTS (
                     SELECT 1 FROM manual_burst_keep WHERE photo_path = ?1 AND photo_fp = ''
                 )",
                rusqlite::params![legacy_path, fp, i64::from(legacy_keep)],
            )?;
        }
        tx.execute(
            "INSERT OR REPLACE INTO decision_meta(key, value) VALUES (?1, '1')",
            [META_BURST_JSON_MIGRATED],
        )?;
        tx.commit()?;
        changed = true;
    }

    if changed {
        db.save()?;
    }
    Ok(())
}

fn match_decision<T: Copy>(root: &Path, key: &str, candidates: &[(String, T)]) -> Option<T> {
    if candidates.is_empty() {
        return None;
    }
    let Ok(fp) = photo_fingerprint(root, key) else {
        return None;
    };
    candidates
        .iter()
        .find(|(stored_fp, _)| !stored_fp.is_empty() && stored_fp == &fp)
        .map(|(_, val)| *val)
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
            if photo_fingerprint(root, &key).is_ok() {
                if let Some(value) = out.get_mut(e.pair_id()) {
                    *value = EffectiveRating {
                        stars,
                        source: RatingSource::Manual,
                    };
                }
            }
        }
    }
    Ok(out)
}

pub fn effective_ratings_checked(
    root: &Path,
    entries: &[PhotoEntry],
    algorithm: &HashMap<String, u8>,
    manual: &DecisionMap<u8>,
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
        if let Some(candidates) = manual.get(&key) {
            if let Some(stars) = match_decision(root, &key, candidates) {
                // 未分析的照片仍未评分；人工覆盖只替代已存在的算法建议。
                if let Some(value) = out.get_mut(e.pair_id()) {
                    *value = EffectiveRating {
                        stars,
                        source: RatingSource::Manual,
                    };
                }
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
            if photo_fingerprint(root, &key).is_ok() {
                if let Some(value) = out.get_mut(e.pair_id()) {
                    if value.group > 0 {
                        value.keep = keep;
                        value.manual_keep = Some(keep);
                    }
                }
            }
        }
    }
    Ok(out)
}

pub fn effective_bursts_checked(
    root: &Path,
    entries: &[PhotoEntry],
    algorithm: &HashMap<String, BurstInfo>,
    manual: &DecisionMap<bool>,
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
        if let Some(candidates) = manual.get(&key) {
            if let Some(keep) = match_decision(root, &key, candidates) {
                if let Some(value) = out.get_mut(e.pair_id()) {
                    if value.group > 0 {
                        value.keep = keep;
                        value.manual_keep = Some(keep);
                    }
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
        std::fs::write(&jpg, b"dummy-a-jpg").unwrap();
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
        std::fs::create_dir_all(root.join("JPG")).unwrap();
        let jpg_a = root.join("JPG").join("a.jpg");
        let jpg_b = root.join("JPG").join("b.jpg");
        let jpg_c = root.join("JPG").join("c.jpg");
        std::fs::write(&jpg_a, b"jpg-a").unwrap();
        std::fs::write(&jpg_b, b"jpg-b").unwrap();
        std::fs::write(&jpg_c, b"jpg-c").unwrap();
        backfill_fingerprints_in(&root.join(".firstcut"), root).unwrap();
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
        std::fs::write(root.join("photo.jpg"), b"dummy-photo").unwrap();

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
        std::fs::write(root.join("photo.jpg"), b"dummy-photo").unwrap();

        let result = set(&root, &root.join("photo.jpg"), 3);
        assert!(result.is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"leave this file alone");

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn swapped_card_same_path_does_not_inherit_manual_rating_or_burst_keep() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("mount");
        let state = temp.path().join("state");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let photo = root.join("IMG_0001.JPG");

        // 卡 A 写入照片内容并打 1 星 + 手动设为舍弃
        std::fs::write(&photo, b"card-a-jpeg-bytes-111").unwrap();
        set_in(&state, &root, &photo, 1).unwrap();
        set_burst_keep_in(&state, &root, &photo, Some(false)).unwrap();

        let entries = vec![PhotoEntry {
            path: photo.display().to_string(),
            pair_id: "p1".into(),
            ..Default::default()
        }];
        let algo_ratings = HashMap::from([("p1".into(), 4)]);
        let algo_bursts = HashMap::from([(
            "p1".into(),
            BurstInfo {
                group: 1,
                size: 2,
                rank: 1,
                keep: true,
                pose_cluster: 1,
            },
        )]);

        let ratings_a =
            effective_ratings_checked(&root, &entries, &algo_ratings, &load_ratings_in(&state).unwrap())
                .unwrap();
        assert_eq!(ratings_a["p1"].stars, 1);
        assert_eq!(ratings_a["p1"].source, RatingSource::Manual);
        let bursts_a =
            effective_bursts_checked(&root, &entries, &algo_bursts, &load_bursts_in(&state).unwrap())
                .unwrap();
        assert!(!bursts_a["p1"].keep);
        assert_eq!(bursts_a["p1"].manual_keep, Some(false));

        // 换卡 B（同一路径，不同照片内容）——绝不能继承卡 A 的人工 1 星和人工舍弃
        std::fs::write(&photo, b"card-b-completely-different-photo-2222").unwrap();
        let ratings_b =
            effective_ratings_checked(&root, &entries, &algo_ratings, &load_ratings_in(&state).unwrap())
                .unwrap();
        assert_eq!(ratings_b["p1"].stars, 4);
        assert_eq!(ratings_b["p1"].source, RatingSource::Algorithm);
        let bursts_b =
            effective_bursts_checked(&root, &entries, &algo_bursts, &load_bursts_in(&state).unwrap())
                .unwrap();
        assert!(bursts_b["p1"].keep);
        assert_eq!(bursts_b["p1"].manual_keep, None);

        // 给卡 B 打 5 星后，换回卡 A 仍能找回卡 A 自己的 1 星
        set_in(&state, &root, &photo, 5).unwrap();
        std::fs::write(&photo, b"card-a-jpeg-bytes-111").unwrap();
        let ratings_a_back =
            effective_ratings_checked(&root, &entries, &algo_ratings, &load_ratings_in(&state).unwrap())
                .unwrap();
        assert_eq!(ratings_a_back["p1"].stars, 1);
        assert_eq!(ratings_a_back["p1"].source, RatingSource::Manual);
    }

    #[test]
    fn missing_photo_fails_save_and_falls_back_to_algorithm_on_match() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("mount");
        let state = temp.path().join("state");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let photo = root.join("DSC00001.JPG");

        std::fs::write(&photo, b"existing-photo-bytes").unwrap();
        set_in(&state, &root, &photo, 1).unwrap();
        set_burst_keep_in(&state, &root, &photo, Some(false)).unwrap();

        // 模拟扫描后照片文件消失：匹配时不得套用人工 1 星 / 手动舍弃，保存必须返回错误且不写空指纹
        std::fs::remove_file(&photo).unwrap();
        let entries = vec![PhotoEntry {
            path: photo.display().to_string(),
            pair_id: "p1".into(),
            ..Default::default()
        }];
        let algo_ratings = HashMap::from([("p1".into(), 4)]);
        let algo_bursts = HashMap::from([(
            "p1".into(),
            BurstInfo {
                group: 1,
                size: 2,
                rank: 1,
                keep: true,
                pose_cluster: 1,
            },
        )]);

        let ratings =
            effective_ratings_checked(&root, &entries, &algo_ratings, &load_ratings_in(&state).unwrap())
                .unwrap();
        assert_eq!(ratings["p1"].stars, 4);
        assert_eq!(ratings["p1"].source, RatingSource::Algorithm);

        let bursts =
            effective_bursts_checked(&root, &entries, &algo_bursts, &load_bursts_in(&state).unwrap())
                .unwrap();
        assert!(bursts["p1"].keep);
        assert_eq!(bursts["p1"].manual_keep, None);

        assert!(set_in(&state, &root, &photo, 5).is_err());
        assert!(set_burst_keep_in(&state, &root, &photo, Some(true)).is_err());
        let stored_ratings = load_ratings_in(&state).unwrap();
        assert!(!stored_ratings["DSC00001.JPG"].iter().any(|(fp, _)| fp.is_empty()));
        assert!(!stored_ratings["DSC00001.JPG"].iter().any(|(_, s)| *s == 5));
    }

    #[test]
    fn migration_preserves_sqlite_priority_over_legacy_burst_json() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("album");
        let state = temp.path().join("state");
        std::fs::create_dir_all(root.join("JPG")).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let photo = root.join("JPG/a.jpg");
        std::fs::write(&photo, b"same-photo-bytes").unwrap();

        // 旧版 2 列 SQLite 标为「舍弃 (0)」，旧版 burst-overrides.json 标为「保留 (true)」
        {
            let conn = rusqlite::Connection::open(state.join("decisions.sqlite")).unwrap();
            conn.execute_batch(
                "CREATE TABLE manual_burst_keep (
                    photo_path TEXT PRIMARY KEY,
                    keep INTEGER NOT NULL CHECK(keep IN (0, 1))
                );
                INSERT INTO manual_burst_keep (photo_path, keep) VALUES ('JPG/a.jpg', 0);",
            )
            .unwrap();
        }
        std::fs::write(
            state.join("burst-overrides.json"),
            r#"{"JPG/a.jpg": true}"#,
        )
        .unwrap();

        // 迁移前：SQLite 优先 => false
        assert_eq!(load_burst_overrides_in(&state).unwrap().get("JPG/a.jpg"), Some(&false));

        // 执行迁移并补齐指纹
        backfill_imported_decisions_in(&state, &root, true, true).unwrap();

        let entries = vec![PhotoEntry {
            path: photo.display().to_string(),
            pair_id: "p1".into(),
            ..Default::default()
        }];
        let algo = HashMap::from([(
            "p1".into(),
            BurstInfo {
                group: 1,
                size: 2,
                rank: 1,
                keep: true,
                pose_cluster: 1,
            },
        )]);
        let eff = effective_bursts_checked(&root, &entries, &algo, &load_bursts_in(&state).unwrap())
            .unwrap();
        assert!(!eff["p1"].keep, "迁移后必须延续 SQLite（舍弃）优先于 JSON（保留）的规则");
        assert_eq!(eff["p1"].manual_keep, Some(false));
    }
}
