//! SQLite 增量缓存（M4）
//!
//! 按 (文件大小, mtime, 分析版本) 缓存像素分析 + AI 结果；
//! 命中则跳过解码与推理，重跑只处理新照片。

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};

use crate::score::{AnalysisResult, PixelScores};

/// 缓存分析版本：评分参数（k 值/权重/模型）变化时递增
pub const CACHE_VERSION: i64 = 7;

/// 照片分析缓存
pub struct ScoreCache {
    conn: rusqlite::Connection,
    /// 预加载的 (path -> 缓存行)
    rows: HashMap<String, CacheRow>,
}

/// 单条缓存行（公开；供并行分析段只读快照使用）
#[derive(Debug, Clone, Copy)]
pub struct CacheRow {
    pub size: i64,
    pub mtime: i64,
    pub version: i64,
    pub result: AnalysisResult,
}

impl ScoreCache {
    /// 打开（或创建）缓存库；失败时返回 Err 由调用方决定降级
    pub fn open(path: &Path) -> Result<Self> {
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
                faces INTEGER NOT NULL
            );",
        )?;

        let rows = {
            let mut stmt = conn.prepare(
                "SELECT path, size, mtime, version, sharpness, exposure, noise, composition, aesthetic, dhash, faces FROM photo_cache",
            )?;
            let iter = stmt.query_map([], |r| {
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
                        },
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
        Ok(ScoreCache { conn, rows })
    }

    /// 只读快照（供 rayon 并行段使用；Connection 非 Sync 不能跨线程）
    pub fn rows(&self) -> &HashMap<String, CacheRow> {
        &self.rows
    }

    /// 查询缓存：文件未变（大小+mtime+版本一致）返回 Some
    pub fn get(&self, path: &str, size: u64, mtime: i64) -> Option<AnalysisResult> {
        let row = self.rows.get(path)?;
        if row.size == size as i64 && row.mtime == mtime && row.version == CACHE_VERSION {
            Some(row.result)
        } else {
            None
        }
    }

    /// 写入（或更新）一条缓存；批量写入后统一 flush
    pub fn put(&mut self, path: &str, size: u64, mtime: i64, result: &AnalysisResult) -> Result<()> {
        self.rows.insert(
            path.to_string(),
            CacheRow {
                size: size as i64,
                mtime,
                version: CACHE_VERSION,
                result: *result,
            },
        );
        Ok(())
    }

    /// 将内存中的全部条目落盘（单事务）
    pub fn flush(&mut self) -> Result<()> {
        let tx = self.conn.transaction()?;
        for (path, row) in &self.rows {
            let s = row.result.scores;
            tx.execute(
                "INSERT OR REPLACE INTO photo_cache
                    (path, size, mtime, version, sharpness, exposure, noise, composition, aesthetic, dhash, faces)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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
                ],
            )?;
        }
        tx.commit()?;
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
