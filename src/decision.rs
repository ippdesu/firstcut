//! 用户复核决定。算法每次可以重算，人工星级与人工连拍保留覆盖统一独立保存在
//! `.firstcut/decisions.sqlite`（并兼容读取/单向迁移旧版 `.firstcut/burst-overrides.json`）。

use std::collections::{HashMap, HashSet};
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

/// 三态人工选片标记（留用 / 未定 / 剔除）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CurationFlag {
    #[default]
    Unflagged,
    Pick,
    Reject,
}

impl CurationFlag {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unflagged => "unflagged",
            Self::Pick => "pick",
            Self::Reject => "reject",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "pick" | "picked" | "p" => Some(Self::Pick),
            "reject" | "rejected" | "x" => Some(Self::Reject),
            "unflagged" | "none" | "clear" | "u" | "" => Some(Self::Unflagged),
            _ => None,
        }
    }

    fn to_db(self) -> Option<i64> {
        match self {
            Self::Pick => Some(1),
            Self::Reject => Some(-1),
            Self::Unflagged => None,
        }
    }

    fn from_db(v: i64) -> Option<Self> {
        match v {
            1 => Some(Self::Pick),
            -1 => Some(Self::Reject),
            _ => None,
        }
    }
}

/// 批量设置选片标记的结果摘要。
#[derive(Debug, Clone, Default)]
pub struct BatchFlagOutcome {
    pub updated: Vec<String>,
    pub failed: Vec<String>,
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

pub(crate) fn photo_fingerprint(root: &Path, rel_key: &str) -> Result<String> {
    let mut file = FileRef::inside(root, &root.join(rel_key))
        .with_context(|| format!("无法安全打开照片文件以生成指纹: {rel_key}"))?
        .open_read()
        .with_context(|| format!("读取照片文件失败: {rel_key}"))?
        .with_context(|| format!("照片文件不存在或不可读，无法生成内容指纹: {rel_key}"))?;
    let len = file
        .metadata()
        .with_context(|| format!("读取照片元数据失败: {rel_key}"))?
        .len();
    fingerprint_opened_file(&mut file, len, rel_key)
}

pub(crate) fn photo_fingerprint_for_source(
    source: &crate::workspace::WorkspaceSource,
    is_raw: bool,
    rel_key: &str,
) -> Result<String> {
    let full = source.photo_full(is_raw, rel_key);
    let mut file = source
        .file_ref_for_photo(is_raw, &full)
        .with_context(|| format!("无法安全打开照片文件以生成指纹: {rel_key}"))?
        .open_read()
        .with_context(|| format!("读取照片文件失败: {rel_key}"))?
        .with_context(|| format!("照片文件不存在或不可读，无法生成内容指纹: {rel_key}"))?;
    let len = file
        .metadata()
        .with_context(|| format!("读取照片元数据失败: {rel_key}"))?
        .len();
    fingerprint_opened_file(&mut file, len, rel_key)
}

fn fingerprint_opened_file<R: Read + Seek>(file: &mut R, len: u64, rel_key: &str) -> Result<String> {
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

fn imported_legacy_needs_flag_upgrade(conn: &Connection) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'imported_legacy_decision' LIMIT 1",
    )?;
    let mut rows = stmt.query([])?;
    let sql = rows.next()?.and_then(|r| r.get::<_, String>(0).ok());
    Ok(sql.is_some_and(|s| !s.contains("'flag'")))
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
        CREATE TABLE IF NOT EXISTS manual_flag (
            photo_path TEXT NOT NULL,
            photo_fp TEXT NOT NULL DEFAULT '',
            flag INTEGER NOT NULL CHECK(flag IN (-1, 1)),
            PRIMARY KEY (photo_path, photo_fp)
        );
        CREATE TABLE IF NOT EXISTS decision_meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS imported_legacy_decision (
            kind TEXT NOT NULL CHECK(kind IN ('rating', 'burst', 'flag')),
            photo_path TEXT NOT NULL,
            photo_fp TEXT NOT NULL,
            PRIMARY KEY (kind, photo_path, photo_fp)
        );",
    )?;
    if imported_legacy_needs_flag_upgrade(conn)? {
        conn.execute_batch(
            "CREATE TABLE imported_legacy_decision_v2 (
                kind TEXT NOT NULL CHECK(kind IN ('rating', 'burst', 'flag')),
                photo_path TEXT NOT NULL,
                photo_fp TEXT NOT NULL,
                PRIMARY KEY (kind, photo_path, photo_fp)
            );
            INSERT INTO imported_legacy_decision_v2 (kind, photo_path, photo_fp)
                SELECT kind, photo_path, photo_fp FROM imported_legacy_decision;
            DROP TABLE imported_legacy_decision;
            ALTER TABLE imported_legacy_decision_v2 RENAME TO imported_legacy_decision;",
        )?;
    }
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
    tx.execute(
        "INSERT OR IGNORE INTO imported_legacy_decision(kind, photo_path, photo_fp) VALUES ('rating', ?1, ?2)",
        rusqlite::params![key, fp],
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
    tx.execute(
        "INSERT OR IGNORE INTO imported_legacy_decision(kind, photo_path, photo_fp) VALUES ('burst', ?1, ?2)",
        rusqlite::params![key, fp],
    )?;
    tx.commit()?;
    db.save()
}

/// 读取人工选片标记（照片相对路径 → `CurationFlag`）。
pub fn load_flags(root: &Path) -> Result<HashMap<String, CurationFlag>> {
    load_flags_in(&root.join(".firstcut"))
}

pub fn load_flags_in(state_dir: &Path) -> Result<HashMap<String, CurationFlag>> {
    let map = load_flag_map_in(state_dir)?;
    Ok(map
        .into_iter()
        .filter_map(|(k, v)| v.last().map(|(_, flag)| (k, *flag)))
        .collect())
}

pub fn load_flag_map_in(state_dir: &Path) -> Result<DecisionMap<CurationFlag>> {
    let state = match Folder::open_or_create_explicit(state_dir, false) {
        Ok(dir) => dir,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(err) => return Err(err.into()),
    };
    let Some(db) = Database::open(state.file(DECISIONS_DB)?, false)? else {
        return Ok(HashMap::new());
    };
    if !table_exists(&db.conn, "manual_flag")? {
        return Ok(HashMap::new());
    }
    let mut out: DecisionMap<CurationFlag> = HashMap::new();
    let mut stmt = db
        .conn
        .prepare("SELECT photo_path, photo_fp, flag FROM manual_flag")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
    })?;
    for row in rows {
        let (path, fp, raw) = row?;
        if let Some(flag) = CurationFlag::from_db(raw) {
            out.entry(path).or_default().push((fp, flag));
        }
    }
    Ok(out)
}

pub fn set_flag(root: &Path, jpg: &Path, flag: CurationFlag) -> Result<()> {
    set_flag_in(&root.join(".firstcut"), root, jpg, flag)
}

pub fn set_flag_in(
    state_dir: &Path,
    jpg_root: &Path,
    jpg: &Path,
    flag: CurationFlag,
) -> Result<()> {
    let key = relative_key(jpg_root, jpg)?;
    let fp = photo_fingerprint(jpg_root, &key)?;
    let state = Folder::open_or_create_explicit(state_dir, true)?;
    let mut db = Database::open(state.file(DECISIONS_DB)?, true)?
        .context("人工评分数据库不可用")?;
    ensure_schema(&db.conn)?;
    let tx = db.conn.transaction()?;
    match flag.to_db() {
        Some(val) => {
            tx.execute(
                "INSERT INTO manual_flag(photo_path, photo_fp, flag) VALUES (?1, ?2, ?3)
                 ON CONFLICT(photo_path, photo_fp) DO UPDATE SET flag = excluded.flag",
                rusqlite::params![&key, &fp, val],
            )?;
        }
        None => {
            tx.execute(
                "DELETE FROM manual_flag WHERE photo_path = ?1 AND photo_fp = ?2",
                rusqlite::params![&key, &fp],
            )?;
        }
    }
    if !fp.is_empty() {
        tx.execute(
            "DELETE FROM manual_flag WHERE photo_path = ?1 AND photo_fp = ''",
            [&key],
        )?;
    }
    tx.execute(
        "INSERT OR IGNORE INTO imported_legacy_decision(kind, photo_path, photo_fp) VALUES ('flag', ?1, ?2)",
        rusqlite::params![&key, &fp],
    )?;
    tx.commit()?;
    db.save()
}

/// 在单次 SQLite 事务内批量写入多张照片的选片标记；任何无法读取内容指纹的照片都会跳过并计入 `failed`。
pub fn set_flags_batch_in(
    state_dir: &Path,
    jpg_root: &Path,
    jpgs: &[std::path::PathBuf],
    flag: CurationFlag,
) -> Result<BatchFlagOutcome> {
    if jpgs.is_empty() {
        return Ok(BatchFlagOutcome::default());
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut verified: Vec<(String, String)> = Vec::with_capacity(jpgs.len());
    let mut failed: Vec<String> = Vec::new();
    for jpg in jpgs {
        let key = match relative_key(jpg_root, jpg) {
            Ok(k) => k,
            Err(_) => {
                failed.push(jpg.display().to_string());
                continue;
            }
        };
        if !seen.insert(key.clone()) {
            continue;
        }
        match photo_fingerprint(jpg_root, &key) {
            Ok(fp) => verified.push((key, fp)),
            Err(_) => failed.push(key),
        }
    }
    if verified.is_empty() {
        anyhow::bail!(
            "所选照片均无法读取内容指纹，未写入选片标记（{} 张失败）",
            failed.len()
        );
    }

    let state = Folder::open_or_create_explicit(state_dir, true)?;
    let mut db = Database::open(state.file(DECISIONS_DB)?, true)?
        .context("人工评分数据库不可用")?;
    ensure_schema(&db.conn)?;
    let tx = db.conn.transaction()?;
    let mut updated = Vec::with_capacity(verified.len());
    for (key, fp) in &verified {
        match flag.to_db() {
            Some(val) => {
                tx.execute(
                    "INSERT INTO manual_flag(photo_path, photo_fp, flag) VALUES (?1, ?2, ?3)
                     ON CONFLICT(photo_path, photo_fp) DO UPDATE SET flag = excluded.flag",
                    rusqlite::params![key, fp, val],
                )?;
            }
            None => {
                tx.execute(
                    "DELETE FROM manual_flag WHERE photo_path = ?1 AND photo_fp = ?2",
                    rusqlite::params![key, fp],
                )?;
            }
        }
        if !fp.is_empty() {
            tx.execute(
                "DELETE FROM manual_flag WHERE photo_path = ?1 AND photo_fp = ''",
                [key],
            )?;
        }
        tx.execute(
            "INSERT OR IGNORE INTO imported_legacy_decision(kind, photo_path, photo_fp) VALUES ('flag', ?1, ?2)",
            rusqlite::params![key, fp],
        )?;
        updated.push(key.clone());
    }
    tx.commit()?;
    db.save()?;
    Ok(BatchFlagOutcome { updated, failed })
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
        || !has_column(&db.conn, "manual_burst_keep", "photo_fp")?
        || !table_exists(&db.conn, "manual_flag")?
        || imported_legacy_needs_flag_upgrade(&db.conn)?;
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
        let flag_rows: Vec<(String, i64)> = if table_exists(&db.conn, "manual_flag")? {
            let mut stmt = db
                .conn
                .prepare("SELECT photo_path, flag FROM manual_flag WHERE photo_fp = ''")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            Vec::new()
        };

        if !rating_rows.is_empty() || !burst_rows.is_empty() || !flag_rows.is_empty() {
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
            for (path, flag) in flag_rows {
                if let Ok(fp) = photo_fingerprint(jpg_root, &path) {
                    tx.execute(
                        "INSERT INTO manual_flag(photo_path, photo_fp, flag) VALUES (?1, ?2, ?3)
                         ON CONFLICT(photo_path, photo_fp) DO NOTHING",
                        rusqlite::params![&path, &fp, flag],
                    )?;
                    tx.execute(
                        "DELETE FROM manual_flag WHERE photo_path = ?1 AND photo_fp = ''",
                        [&path],
                    )?;
                    changed = true;
                }
            }
            tx.commit()?;
        }
    }

    // Step B: 再导入本次的 burst-overrides.json（Step A 已为同卡 SQLite 记录补齐指纹，主键冲突即可保证 SQLite 优先；
    // 且不因旧卡遗留的空指纹记录挡住新卡有效照片）
    if import_burst_json && has_legacy_json && !is_burst_json_migrated(&db.conn)? {
        let legacy_map = load_legacy_burst_json(&state)?;
        let tx = db.conn.transaction()?;
        for (legacy_path, legacy_keep) in legacy_map {
            let fp = photo_fingerprint(jpg_root, &legacy_path).unwrap_or_default();
            tx.execute(
                "INSERT OR IGNORE INTO manual_burst_keep(photo_path, photo_fp, keep) VALUES (?1, ?2, ?3)",
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

/// 按单条 `(kind, photo_path, photo_fp)` 幂等合并旧版 `<相册>/.firstcut/` 中的
/// `decisions.sqlite` 与 `burst-overrides.json`：
/// - 同一挂载点复用不同旧卡时，每张卡的旧 SQLite/JSON 都能独立迁移并按照片内容指纹隔离；
/// - 即使后续向相册增删其他照片，已导入或已清除的 `(kind, path, fp)` 仍记录在 `imported_legacy_decision`，绝不复活旧决定；
/// - 同照片冲突时严格保持 SQLite 优先于旧 JSON，且绝不被旧卡遗留的空指纹记录阻挡。
pub fn import_legacy_decisions_in(
    state_dir: &Path,
    jpg_root: &Path,
    legacy_folder: &Folder,
) -> Result<()> {
    let mut db_ratings: Vec<(String, String, u8)> = Vec::new();
    let mut db_bursts: Vec<(String, String, i64)> = Vec::new();
    let mut db_flags: Vec<(String, String, i64)> = Vec::new();
    let mut legacy_db_burst_migrated = false;

    if let Ok(src_ref) = legacy_folder.file(DECISIONS_DB) {
        if let Ok(Some(legacy_db)) = Database::open(src_ref, false) {
            legacy_db_burst_migrated = is_burst_json_migrated(&legacy_db.conn).unwrap_or(false);

            if table_exists(&legacy_db.conn, "manual_rating")? {
                if has_column(&legacy_db.conn, "manual_rating", "photo_fp")? {
                    let mut stmt = legacy_db
                        .conn
                        .prepare("SELECT photo_path, photo_fp, stars FROM manual_rating")?;
                    let rows = stmt.query_map([], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, u8>(2)?))
                    })?;
                    for row in rows {
                        let (path, stored_fp, stars) = row?;
                        let fp = if stored_fp.is_empty() {
                            photo_fingerprint(jpg_root, &path).unwrap_or_default()
                        } else {
                            stored_fp
                        };
                        db_ratings.push((path, fp, stars));
                    }
                } else {
                    let mut stmt = legacy_db
                        .conn
                        .prepare("SELECT photo_path, stars FROM manual_rating")?;
                    let rows = stmt
                        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u8>(1)?)))?;
                    for row in rows {
                        let (path, stars) = row?;
                        let fp = photo_fingerprint(jpg_root, &path).unwrap_or_default();
                        db_ratings.push((path, fp, stars));
                    }
                }
            }

            if table_exists(&legacy_db.conn, "manual_burst_keep")? {
                if has_column(&legacy_db.conn, "manual_burst_keep", "photo_fp")? {
                    let mut stmt = legacy_db
                        .conn
                        .prepare("SELECT photo_path, photo_fp, keep FROM manual_burst_keep")?;
                    let rows = stmt.query_map([], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
                    })?;
                    for row in rows {
                        let (path, stored_fp, keep) = row?;
                        let fp = if stored_fp.is_empty() {
                            photo_fingerprint(jpg_root, &path).unwrap_or_default()
                        } else {
                            stored_fp
                        };
                        db_bursts.push((path, fp, keep));
                    }
                } else {
                    let mut stmt = legacy_db
                        .conn
                        .prepare("SELECT photo_path, keep FROM manual_burst_keep")?;
                    let rows = stmt
                        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
                    for row in rows {
                        let (path, keep) = row?;
                        let fp = photo_fingerprint(jpg_root, &path).unwrap_or_default();
                        db_bursts.push((path, fp, keep));
                    }
                }
            }

            if table_exists(&legacy_db.conn, "manual_flag")? {
                let mut stmt = legacy_db
                    .conn
                    .prepare("SELECT photo_path, photo_fp, flag FROM manual_flag")?;
                let rows = stmt.query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
                })?;
                for row in rows {
                    let (path, stored_fp, raw_flag) = row?;
                    if CurationFlag::from_db(raw_flag).is_none() {
                        continue;
                    }
                    let fp = if stored_fp.is_empty() {
                        photo_fingerprint(jpg_root, &path).unwrap_or_default()
                    } else {
                        stored_fp
                    };
                    db_flags.push((path, fp, raw_flag));
                }
            }
        }
    }

    let mut json_bursts: Vec<(String, String, i64)> = Vec::new();
    if !legacy_db_burst_migrated {
        if let Ok(Some(json_bytes)) = legacy_folder
            .file(LEGACY_BURST_JSON)
            .and_then(|f| f.read(4 * 1024 * 1024))
        {
            if let Ok(map) = serde_json::from_slice::<HashMap<String, bool>>(&json_bytes) {
                for (path, keep) in map {
                    let fp = photo_fingerprint(jpg_root, &path).unwrap_or_default();
                    json_bursts.push((path, fp, i64::from(keep)));
                }
            }
        }
    }

    if db_ratings.is_empty() && db_bursts.is_empty() && db_flags.is_empty() && json_bursts.is_empty() {
        return Ok(());
    }

    let state = Folder::open_or_create_explicit(state_dir, true)?;
    let mut db = Database::open(state.file(DECISIONS_DB)?, true)?
        .context("人工评分数据库不可用")?;
    let upgraded_schema = !table_exists(&db.conn, "manual_rating")?
        || !has_column(&db.conn, "manual_rating", "photo_fp")?
        || !table_exists(&db.conn, "manual_burst_keep")?
        || !has_column(&db.conn, "manual_burst_keep", "photo_fp")?
        || !table_exists(&db.conn, "manual_flag")?
        || !table_exists(&db.conn, "imported_legacy_decision")?
        || imported_legacy_needs_flag_upgrade(&db.conn)?;
    ensure_schema(&db.conn)?;

    let mut changed = upgraded_schema;
    let tx = db.conn.transaction()?;
    for (path, fp, stars) in db_ratings {
        if tx.execute(
            "INSERT OR IGNORE INTO imported_legacy_decision(kind, photo_path, photo_fp) VALUES ('rating', ?1, ?2)",
            rusqlite::params![&path, &fp],
        )? > 0
        {
            tx.execute(
                "INSERT OR IGNORE INTO manual_rating(photo_path, photo_fp, stars) VALUES (?1, ?2, ?3)",
                rusqlite::params![&path, &fp, stars],
            )?;
            changed = true;
        }
    }
    for (path, fp, keep) in db_bursts {
        if tx.execute(
            "INSERT OR IGNORE INTO imported_legacy_decision(kind, photo_path, photo_fp) VALUES ('burst', ?1, ?2)",
            rusqlite::params![&path, &fp],
        )? > 0
        {
            tx.execute(
                "INSERT OR IGNORE INTO manual_burst_keep(photo_path, photo_fp, keep) VALUES (?1, ?2, ?3)",
                rusqlite::params![&path, &fp, keep],
            )?;
            changed = true;
        }
    }
    for (path, fp, raw_flag) in db_flags {
        if tx.execute(
            "INSERT OR IGNORE INTO imported_legacy_decision(kind, photo_path, photo_fp) VALUES ('flag', ?1, ?2)",
            rusqlite::params![&path, &fp],
        )? > 0
        {
            tx.execute(
                "INSERT OR IGNORE INTO manual_flag(photo_path, photo_fp, flag) VALUES (?1, ?2, ?3)",
                rusqlite::params![&path, &fp, raw_flag],
            )?;
            changed = true;
        }
    }
    for (path, fp, keep) in json_bursts {
        if tx.execute(
            "INSERT OR IGNORE INTO imported_legacy_decision(kind, photo_path, photo_fp) VALUES ('burst', ?1, ?2)",
            rusqlite::params![&path, &fp],
        )? > 0
        {
            tx.execute(
                "INSERT OR IGNORE INTO manual_burst_keep(photo_path, photo_fp, keep) VALUES (?1, ?2, ?3)",
                rusqlite::params![&path, &fp, keep],
            )?;
            changed = true;
        }
    }
    tx.commit()?;
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
                out.insert(
                    e.pair_id().to_string(),
                    EffectiveRating {
                        stars,
                        source: RatingSource::Manual,
                    },
                );
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
                // 未分析的照片也允许保留并生效人工打星（按内容指纹校验）。
                out.insert(
                    e.pair_id().to_string(),
                    EffectiveRating {
                        stars,
                        source: RatingSource::Manual,
                    },
                );
            }
        }
    }
    Ok(out)
}

/// 按内容指纹校验合并人工选片标记，输出 `pair_id -> CurationFlag`（未定不写入映射）。
pub fn effective_flags_checked(
    root: &Path,
    entries: &[PhotoEntry],
    manual: &DecisionMap<CurationFlag>,
) -> Result<HashMap<String, CurationFlag>> {
    let mut out: HashMap<String, CurationFlag> = HashMap::new();
    for e in entries.iter().filter(|e| !e.is_raw) {
        let key = relative_key(root, Path::new(&e.path))?;
        if let Some(candidates) = manual.get(&key) {
            if let Some(flag) = match_decision(root, &key, candidates) {
                if flag != CurationFlag::Unflagged {
                    out.insert(e.pair_id().to_string(), flag);
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

    #[test]
    fn curation_flag_persists_survives_swapped_card_and_propagates_to_raw() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("mount");
        let state = temp.path().join("state");
        std::fs::create_dir_all(root.join("JPG")).unwrap();
        std::fs::create_dir_all(root.join("RAW")).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let jpg_a = root.join("JPG/DSC00001.JPG");
        let raw_a = root.join("RAW/DSC00001.ARW");
        std::fs::write(&jpg_a, b"card-a-jpg-bytes").unwrap();
        std::fs::write(&raw_a, b"card-a-raw-bytes").unwrap();

        set_flag_in(&state, &root, &jpg_a, CurationFlag::Pick).unwrap();
        let entries = vec![
            PhotoEntry {
                path: jpg_a.display().to_string(),
                pair_id: "p1".into(),
                ..Default::default()
            },
            PhotoEntry {
                path: raw_a.display().to_string(),
                pair_id: "p1".into(),
                is_raw: true,
                ..Default::default()
            },
        ];
        let eff_a = effective_flags_checked(&root, &entries, &load_flag_map_in(&state).unwrap()).unwrap();
        assert_eq!(eff_a.get("p1").copied(), Some(CurationFlag::Pick));

        // 换卡 B：同路径不同内容，绝不继承卡 A 的留用标记
        std::fs::write(&jpg_a, b"card-b-completely-different-photo").unwrap();
        let eff_b = effective_flags_checked(&root, &entries, &load_flag_map_in(&state).unwrap()).unwrap();
        assert!(!eff_b.contains_key("p1"));

        // 卡 B 标记为剔除后，换回卡 A 仍保留卡 A 自己的留用标记
        set_flag_in(&state, &root, &jpg_a, CurationFlag::Reject).unwrap();
        std::fs::write(&jpg_a, b"card-a-jpg-bytes").unwrap();
        let eff_a_back = effective_flags_checked(&root, &entries, &load_flag_map_in(&state).unwrap()).unwrap();
        assert_eq!(eff_a_back.get("p1").copied(), Some(CurationFlag::Pick));

        // 取消标记后清除，且对缺失照片 fail-closed 报错
        set_flag_in(&state, &root, &jpg_a, CurationFlag::Unflagged).unwrap();
        assert!(effective_flags_checked(&root, &entries, &load_flag_map_in(&state).unwrap())
            .unwrap()
            .is_empty());
        std::fs::remove_file(&jpg_a).unwrap();
        assert!(set_flag_in(&state, &root, &jpg_a, CurationFlag::Pick).is_err());
    }

    #[test]
    fn batch_flag_updates_valid_photos_in_one_tx_and_skips_missing_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("album");
        let state = temp.path().join("state");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let p1 = root.join("a.jpg");
        let p2 = root.join("b.jpg");
        let missing = root.join("missing.jpg");
        std::fs::write(&p1, b"photo-a").unwrap();
        std::fs::write(&p2, b"photo-b").unwrap();

        let out = set_flags_batch_in(
            &state,
            &root,
            &[p1.clone(), p2.clone(), p1.clone(), missing.clone()],
            CurationFlag::Pick,
        )
        .unwrap();
        assert_eq!(out.updated, vec!["a.jpg".to_string(), "b.jpg".to_string()]);
        assert_eq!(out.failed, vec!["missing.jpg".to_string()]);

        let map = load_flag_map_in(&state).unwrap();
        assert!(!map.contains_key("missing.jpg"), "缺失照片不得写入空指纹记录");
        assert_eq!(load_flags_in(&state).unwrap().get("a.jpg"), Some(&CurationFlag::Pick));
        assert_eq!(load_flags_in(&state).unwrap().get("b.jpg"), Some(&CurationFlag::Pick));

        // 全部失败时返回 Err
        assert!(set_flags_batch_in(&state, &root, &[missing], CurationFlag::Reject).is_err());
    }

    #[test]
    fn flag_upgrades_existing_pr17_imported_legacy_decision_check_constraint() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("album");
        let state = temp.path().join("state");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let photo = root.join("DSC00001.JPG");
        std::fs::write(&photo, b"photo-bytes").unwrap();

        // 预置 PR #17 旧版 CHECK(kind IN ('rating', 'burst')) 的数据库
        {
            let conn = rusqlite::Connection::open(state.join("decisions.sqlite")).unwrap();
            conn.execute_batch(
                "CREATE TABLE manual_rating (
                    photo_path TEXT NOT NULL,
                    photo_fp TEXT NOT NULL DEFAULT '',
                    stars INTEGER NOT NULL CHECK(stars BETWEEN 1 AND 5),
                    PRIMARY KEY (photo_path, photo_fp)
                );
                CREATE TABLE manual_burst_keep (
                    photo_path TEXT NOT NULL,
                    photo_fp TEXT NOT NULL DEFAULT '',
                    keep INTEGER NOT NULL CHECK(keep IN (0, 1)),
                    PRIMARY KEY (photo_path, photo_fp)
                );
                CREATE TABLE decision_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                CREATE TABLE imported_legacy_decision (
                    kind TEXT NOT NULL CHECK(kind IN ('rating', 'burst')),
                    photo_path TEXT NOT NULL,
                    photo_fp TEXT NOT NULL,
                    PRIMARY KEY (kind, photo_path, photo_fp)
                );
                INSERT INTO imported_legacy_decision(kind, photo_path, photo_fp)
                    VALUES ('rating', 'DSC00001.JPG', 'legacy-fp');",
            )
            .unwrap();
        }

        set_flag_in(&state, &root, &photo, CurationFlag::Pick).unwrap();
        assert_eq!(load_flags_in(&state).unwrap().get("DSC00001.JPG"), Some(&CurationFlag::Pick));

        let conn = rusqlite::Connection::open(state.join("decisions.sqlite")).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM imported_legacy_decision WHERE kind = 'rating' AND photo_fp = 'legacy-fp'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "升级 CHECK 约束时必须保留既有迁移记录");
    }

    #[test]
    fn unscored_manual_rating_surfaces_in_effective_ratings_checked() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("album");
        let state = temp.path().join("state");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let photo = root.join("unscored.jpg");
        std::fs::write(&photo, b"unscored-photo-bytes").unwrap();

        set_in(&state, &root, &photo, 5).unwrap();
        let entries = vec![PhotoEntry {
            path: photo.display().to_string(),
            pair_id: "p_unscored".into(),
            ..Default::default()
        }];
        let empty_algo: HashMap<String, u8> = HashMap::new();
        let eff = effective_ratings_checked(
            &root,
            &entries,
            &empty_algo,
            &load_ratings_in(&state).unwrap(),
        )
        .unwrap();
        assert_eq!(
            eff.get("p_unscored").copied(),
            Some(EffectiveRating {
                stars: 5,
                source: RatingSource::Manual,
            })
        );

        // 换卡不同内容后不得继承未评分的人工星级
        std::fs::write(&photo, b"swapped-card-photo-bytes").unwrap();
        let eff_swapped = effective_ratings_checked(
            &root,
            &entries,
            &empty_algo,
            &load_ratings_in(&state).unwrap(),
        )
        .unwrap();
        assert!(!eff_swapped.contains_key("p_unscored"));
    }
}
