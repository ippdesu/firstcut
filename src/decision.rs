//! 用户复核决定。算法每次可以重算，人工星级独立保存。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::scan::PhotoEntry;

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

/// 决定存储在照片根目录内，独立于可删除的分析缓存。
///
/// `.firstcut` 可能来自照片目录本身，因此解析链接后必须确认它仍位于所选根目录内。
fn store_path(root: &Path, create_state_dir: bool) -> Result<PathBuf> {
    let root = root.canonicalize()?;
    let state_dir = root.join(".firstcut");
    let metadata = match state_dir.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && !create_state_dir => {
            return Ok(state_dir.join("decisions.sqlite"));
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            // create_dir is intentionally non-recursive: if a link appears after the
            // metadata check, it fails instead of following that link to another folder.
            match std::fs::create_dir(&state_dir) {
                Ok(()) => state_dir.symlink_metadata()?,
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    state_dir.symlink_metadata()?
                }
                Err(err) => return Err(err.into()),
            }
        }
        Err(err) => return Err(err.into()),
    };
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "拒绝将 firstcut 状态目录作为符号链接或非目录使用"
    );
    let state_dir = state_dir.canonicalize()?;
    anyhow::ensure!(
        state_dir.starts_with(&root),
        "firstcut 状态目录必须位于照片根目录内"
    );

    let path = state_dir.join("decisions.sqlite");
    match path.symlink_metadata() {
        Ok(metadata) => anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "拒绝写入符号链接形式的人工评分数据库"
        ),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    Ok(path)
}

fn relative_key(root: &Path, photo: &Path) -> Result<String> {
    Ok(photo
        .strip_prefix(root)
        .with_context(|| format!("照片不在根目录内: {}", photo.display()))?
        .to_string_lossy()
        .replace('\\', "/"))
}

pub fn load(root: &Path) -> Result<HashMap<String, u8>> {
    let path = store_path(root, false)?;
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let conn = rusqlite::Connection::open(&path)?;
    let mut stmt = conn.prepare("SELECT photo_path, stars FROM manual_rating")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u8>(1)?)))?;
    rows.collect::<rusqlite::Result<HashMap<_, _>>>()
        .map_err(Into::into)
}

pub fn set(root: &Path, jpg: &Path, stars: u8) -> Result<()> {
    anyhow::ensure!((1..=5).contains(&stars), "星级必须在 1~5");
    let key = relative_key(root, jpg)?;
    let path = store_path(root, true)?;
    let conn = rusqlite::Connection::open(path)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS manual_rating (
        photo_path TEXT PRIMARY KEY, stars INTEGER NOT NULL CHECK(stars BETWEEN 1 AND 5)
    )",
    )?;
    conn.execute(
        "INSERT INTO manual_rating(photo_path, stars) VALUES (?1, ?2)
        ON CONFLICT(photo_path) DO UPDATE SET stars=excluded.stars",
        rusqlite::params![key, stars],
    )?;
    Ok(())
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
