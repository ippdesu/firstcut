//! SQLite 增量缓存（M4）
//!
//! 按 (文件大小, mtime, 分析版本, 配置指纹) 缓存像素分析 + AI 结果；
//! 命中则跳过解码与推理，重跑只处理新照片。
//! 配置指纹用于让 `--config` 改动后自动失效旧分数（否则改配置看不到变化）。

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};

use crate::score::{AnalysisResult, PixelScores};

/// 缓存分析版本：评分参数（k 值/权重/模型）变化时递增
///
/// 13 = M9：缓存行增加姿态描述子（SCRFD 关键点），旧行全部失效重建
pub const CACHE_VERSION: i64 = 15;

/// 照片分析缓存
pub struct ScoreCache {
    db: crate::storage::Database,
    /// 预加载的 (path -> 缓存行)
    rows: HashMap<String, CacheRow>,
    /// 本次运行使用的评分配置指纹（参与缓存键）
    cfg_hash: i64,
    /// 本次新增/更新的行（flush 只写这些）
    dirty: std::collections::HashSet<String>,
}

/// 单条缓存行（公开；供并行分析段只读快照使用）
#[derive(Debug, Clone, Copy)]
pub struct CacheRow {
    pub size: i64,
    pub mtime: i64,
    pub version: i64,
    /// 生成该行时使用的评分配置指纹
    pub cfg_hash: i64,
    pub result: AnalysisResult,
}

impl CacheRow {
    /// 缓存命中判定：文件未变且分析版本、配置指纹一致
    ///
    /// 收敛到这里，避免 score.rs 内联一份、cache.rs 再有一份而将来改漏。
    pub fn matches(&self, size: u64, mtime: i64, version: i64, cfg_hash: i64) -> bool {
        self.size == size as i64
            && self.mtime == mtime
            && self.version == version
            && self.cfg_hash == cfg_hash
    }
}

impl ScoreCache {
    /// 打开（或创建）缓存库；`cfg_hash` 为本次运行的评分配置指纹
    pub fn open(path: &Path, cfg_hash: i64) -> Result<Self> {
        Self::open_at(path, crate::storage::FileRef::explicit(path, false)?, cfg_hash, true)?
            .context("评分库不可用")
    }

    pub(crate) fn open_in(root: &Path, path: &Path, cfg_hash: i64, write: bool) -> Result<Option<Self>> {
        if write {
            crate::storage::Folder::open_or_create_explicit(root, true)?;
        }
        let destination = match crate::storage::FileRef::at(root, path, write) {
            Ok(file) => file,
            Err(err) if !write && err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        Self::open_at(path, destination, cfg_hash, write)
    }

    fn open_at(path: &Path, file: crate::storage::FileRef, cfg_hash: i64, write: bool) -> Result<Option<Self>> {
        let Some(db) = crate::storage::Database::open(file, write)
            .with_context(|| format!("打开缓存库失败: {}", path.display()))?
        else {
            return Ok(None);
        };
        let conn = &db.conn;
        if write {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS photo_cache (
                path TEXT PRIMARY KEY,
                size INTEGER NOT NULL,
                mtime INTEGER NOT NULL,
                version INTEGER NOT NULL,
                sharpness REAL NOT NULL,
                exposure REAL NOT NULL,
                noise REAL NOT NULL,
                composition REAL NOT NULL,
                aesthetic REAL NOT NULL,
                suggested_ev REAL,
                dhash INTEGER NOT NULL,
                faces INTEGER NOT NULL,
                cfg_hash INTEGER NOT NULL DEFAULT 0,
                pose_desc BLOB
            );",
        )?;
        // 旧库迁移：缺列就补（失败说明已存在，忽略）
        let _ = conn.execute(
            "ALTER TABLE photo_cache ADD COLUMN cfg_hash INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = conn.execute("ALTER TABLE photo_cache ADD COLUMN pose_desc BLOB", []);
        let _ = conn.execute("ALTER TABLE photo_cache ADD COLUMN suggested_ev REAL", []);
        }

        let rows = {
            let mut stmt = conn.prepare(
                "SELECT path, size, mtime, version, sharpness, exposure, noise, composition, aesthetic, suggested_ev, dhash, faces, cfg_hash, pose_desc FROM photo_cache",
            )?;
            let iter = stmt.query_map([], |r| {
                let pose_desc: Option<Vec<u8>> = r.get(13)?;
                let suggested_ev: Option<f64> = r.get(9)?;
                Ok((
                    r.get::<_, String>(0)?,
                    CacheRow {
                        size: r.get(1)?,
                        mtime: r.get(2)?,
                        version: r.get(3)?,
                        result: AnalysisResult {
                            scores: PixelScores {
                                sharpness: r.get(4)?,
                                exposure: r.get(5)?,
                                noise: r.get(6)?,
                                composition: r.get(7)?,
                                aesthetic: r.get(8)?,
                            },
                            dhash: r.get::<_, i64>(10)? as u64,
                            faces: usize::try_from(r.get::<_, i64>(11)?).map_err(|err|
                                rusqlite::Error::FromSqlConversionFailure(11, rusqlite::types::Type::Integer, Box::new(err)))?,
                            suggested_ev,
                            pose_desc: pose_desc.as_deref().and_then(blob_to_desc),
                        },
                        cfg_hash: r.get(12)?,
                    },
                ))
            })?;
            let mut rows = HashMap::new();
            for row in iter {
                let (path, row) = row?;
                rows.insert(path, row);
            }
            rows
        };
        Ok(Some(ScoreCache { db, rows, cfg_hash, dirty: std::collections::HashSet::new() }))
    }

    /// 只读快照（供 rayon 并行段使用；Connection 非 Sync 不能跨线程）
    pub fn rows(&self) -> &HashMap<String, CacheRow> {
        &self.rows
    }

    /// 写入（或更新）一条缓存；批量写入后统一 flush
    pub fn put(&mut self, path: &str, size: u64, mtime: i64, result: &AnalysisResult) -> Result<()> {
        self.rows.insert(
            path.to_string(),
            CacheRow {
                size: size as i64,
                mtime,
                version: CACHE_VERSION,
                cfg_hash: self.cfg_hash,
                result: *result,
            },
        );
        self.dirty.insert(path.to_string());
        Ok(())
    }

    /// 只把本次新增/更新的行落盘（单事务）
    ///
    /// 旧实现每次 flush 都会把内存中全部行重写一遍（含未变化的旧行），
    /// 上万张时是纯粹的浪费。
    pub fn flush(&mut self) -> Result<()> {
        if self.dirty.is_empty() {
            return Ok(());
        }
        let tx = self.db.conn.transaction()?;
        for path in &self.dirty {
            let Some(row) = self.rows.get(path) else { continue };
            let s = row.result.scores;
            let pose_desc = row.result.pose_desc.map(desc_to_blob);
            tx.execute(
                "INSERT OR REPLACE INTO photo_cache
                    (path, size, mtime, version, sharpness, exposure, noise, composition, aesthetic, suggested_ev, dhash, faces, cfg_hash, pose_desc)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                rusqlite::params![
                    path,
                    row.size,
                    row.mtime,
                    row.version,
                    s.sharpness,
                    s.exposure,
                    s.noise,
                    s.composition,
                    s.aesthetic,
                    row.result.suggested_ev,
                    row.result.dhash as i64,
                    row.result.faces as i64,
                    row.cfg_hash,
                    pose_desc,
                ],
            )?;
        }
        tx.commit()?;
        self.db.save()?;
        self.dirty.clear();
        Ok(())
    }

    /// 缓存条目数
    pub fn len(&self) -> usize {
        self.rows.len()
    }
}

/// 可移植缓存键：相对照片根目录的正斜杠路径（跨目录搬迁与 Windows/Docker 通用）。
pub fn cache_key(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path).to_string_lossy();
    #[cfg(windows)]
    {
        rel.replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        rel.into_owned()
    }
}

/// 按条件查找缓存行：先校验可移植相对路径键，未命中或条件不满足时再回退校验旧版绝对路径键。
/// 避免一条已失效的相对路径记录挡住仍有效的旧版绝对路径记录。
/// 返回 `(行引用, 是否命中旧版绝对路径键)`。
pub fn find_matching<'a, F>(
    rows: &'a HashMap<String, CacheRow>,
    root: &Path,
    full_path: &str,
    mut predicate: F,
) -> Option<(&'a CacheRow, bool)>
where
    F: FnMut(&CacheRow) -> bool,
{
    let key = cache_key(root, Path::new(full_path));
    if let Some(row) = rows.get(&key).filter(|row| predicate(row)) {
        return Some((row, false));
    }
    if key != full_path {
        if let Some(row) = rows.get(full_path).filter(|row| predicate(row)) {
            return Some((row, true));
        }
    }
    None
}

/// 文件元数据：大小 + mtime（纳秒精度）
pub fn file_fingerprint(path: &Path) -> Option<(u64, i64)> {
    let meta = std::fs::metadata(path).ok()?;
    metadata_fingerprint(&meta)
}

pub fn metadata_fingerprint(meta: &std::fs::Metadata) -> Option<(u64, i64)> {
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos() as i64;
    Some((meta.len(), mtime))
}

/// 姿态描述子 → BLOB（10 × f32 小端，40 字节）
fn desc_to_blob(d: [f32; 10]) -> Vec<u8> {
    let mut out = Vec::with_capacity(40);
    for v in d {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// BLOB → 姿态描述子；长度不符（损坏行）按 None 处理
fn blob_to_desc(bytes: &[u8]) -> Option<[f32; 10]> {
    if bytes.len() != 40 {
        return None;
    }
    let mut d = [0.0f32; 10];
    for (i, v) in d.iter_mut().enumerate() {
        *v = f32::from_le_bytes([bytes[i * 4], bytes[i * 4 + 1], bytes[i * 4 + 2], bytes[i * 4 + 3]]);
    }
    Some(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_is_relative_and_lookup_falls_back_to_legacy_absolute_path() {
        let root = Path::new("/photos/event");
        let full = "/photos/event/JPG/DSC00001.JPG";
        assert_eq!(cache_key(root, Path::new(full)), "JPG/DSC00001.JPG");

        let sample = CacheRow {
            size: 100,
            mtime: 200,
            version: CACHE_VERSION,
            cfg_hash: 42,
            result: AnalysisResult {
                scores: PixelScores::default(),
                dhash: 1,
                faces: 0,
                pose_desc: None,
                suggested_ev: None,
            },
        };
        let mut legacy_rows = HashMap::new();
        legacy_rows.insert(full.to_string(), sample);
        let (found, was_legacy) = find_matching(&legacy_rows, root, full, |r| r.cfg_hash == 42).unwrap();
        assert_eq!(found.cfg_hash, 42);
        assert!(was_legacy);

        let mut portable_rows = HashMap::new();
        portable_rows.insert("JPG/DSC00001.JPG".to_string(), sample);
        let (found, was_legacy) = find_matching(
            &portable_rows,
            Path::new("/moved/event"),
            "/moved/event/JPG/DSC00001.JPG",
            |r| r.cfg_hash == 42,
        )
        .unwrap();
        assert_eq!(found.cfg_hash, 42);
        assert!(!was_legacy);

        // 相对路径行指纹不匹配时，不应挡住仍有效的旧版绝对路径行
        let mut stale_relative = sample;
        stale_relative.cfg_hash = 999;
        let mut both_rows = HashMap::new();
        both_rows.insert("JPG/DSC00001.JPG".to_string(), stale_relative);
        both_rows.insert(full.to_string(), sample);
        let (found, was_legacy) =
            find_matching(&both_rows, root, full, |r| r.matches(100, 200, CACHE_VERSION, 42))
                .unwrap();
        assert_eq!(found.cfg_hash, 42);
        assert!(was_legacy);
    }
}
