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
pub const CACHE_VERSION: i64 = 13;

/// 照片分析缓存
pub struct ScoreCache {
    conn: rusqlite::Connection,
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
        let conn = rusqlite::Connection::open(path)
            .with_context(|| format!("打开缓存库失败: {}", path.display()))?;
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

        let rows = {
            let mut stmt = conn.prepare(
                "SELECT path, size, mtime, version, sharpness, exposure, noise, composition, aesthetic, dhash, faces, cfg_hash, pose_desc FROM photo_cache",
            )?;
            let iter = stmt.query_map([], |r| {
                let pose_desc: Option<Vec<u8>> = r.get(12)?;
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
                            dhash: r.get::<_, i64>(9)? as u64,
                            faces: r.get(10)?,
                            pose_desc: pose_desc.as_deref().and_then(blob_to_desc),
                        },
                        cfg_hash: r.get(11)?,
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
        Ok(ScoreCache { conn, rows, cfg_hash, dirty: std::collections::HashSet::new() })
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
        let tx = self.conn.transaction()?;
        for path in &self.dirty {
            let Some(row) = self.rows.get(path) else { continue };
            let s = row.result.scores;
            let pose_desc = row.result.pose_desc.map(desc_to_blob);
            tx.execute(
                "INSERT OR REPLACE INTO photo_cache
                    (path, size, mtime, version, sharpness, exposure, noise, composition, aesthetic, dhash, faces, cfg_hash, pose_desc)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
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
                    row.result.dhash as i64,
                    row.result.faces as i64,
                    row.cfg_hash,
                    pose_desc,
                ],
            )?;
        }
        tx.commit()?;
        self.dirty.clear();
        Ok(())
    }

    /// 缓存条目数
    pub fn len(&self) -> usize {
        self.rows.len()
    }
}

/// 文件元数据：大小 + mtime（纳秒精度）
pub fn file_fingerprint(path: &Path) -> Option<(u64, i64)> {
    let meta = std::fs::metadata(path).ok()?;
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
