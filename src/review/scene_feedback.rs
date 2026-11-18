//! 逐张场景标注记录。JSONL 保留每次修正及当时的评分线索，便于后续校准。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::snapshot::ScoresJson;
use crate::storage::FileRef;

pub const MAX_FEEDBACK_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SceneFeedbackRecord {
    pub photo_path: String,
    pub predicted_scene: String,
    pub predicted_reason: String,
    pub selected_scene: String,
    pub note: String,
    pub recorded_at_unix_ms: u128,
    pub faces: usize,
    pub analysis_mode: Option<String>,
    pub scores: Option<ScoresJson>,
}

pub fn store_path(root: &Path) -> PathBuf {
    root.join(".firstcut").join("scene-feedback.jsonl")
}

pub fn valid_scene(scene: &str) -> bool {
    matches!(scene, "portrait" | "stage" | "highkey" | "sports" | "lowlight"
        | "landscape" | "still_life" | "documentary" | "other")
}

pub fn now_unix_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis()
}

pub fn load_latest(root: &Path, path: &Path) -> Result<HashMap<String, SceneFeedbackRecord>> {
    let Some(file) = FileRef::inside(root, path)?.open_read()
        .with_context(|| format!("读取场景反馈失败：{}", path.display()))? else {
        return Ok(HashMap::new());
    };
    // Preserve oversized legacy files for export. Only the bounded prefix
    // is available to the live UI until the owner archives the file.
    if file.metadata()?.len() > MAX_FEEDBACK_BYTES {
        super::quiet_log("[review] 场景反馈超过读取上限；原文件已保留，请手动归档");
    }
    let mut latest = HashMap::new();
    for (index, line) in BufReader::new(file.take(MAX_FEEDBACK_BYTES)).split(b'\n').enumerate() {
        let line = line.with_context(|| format!("读取场景反馈第 {} 行失败", index + 1))?;
        let Ok(line) = std::str::from_utf8(&line) else { continue; };
        if line.trim().is_empty() { continue; }
        match serde_json::from_str::<SceneFeedbackRecord>(&line) {
            Ok(record) => { latest.insert(record.photo_path.clone(), record); }
            Err(err) => super::quiet_log(format!("[review] 场景反馈第 {} 行无效，已跳过：{err}", index + 1)),
        }
    }
    Ok(latest)
}

pub fn append(root: &Path, path: &Path, record: &SceneFeedbackRecord) -> Result<()> {
    let mut line = serde_json::to_vec(record)?;
    line.push(b'\n');
    let target = FileRef::at(root, path, true)?;
    let mut file = target.open_update()
        .with_context(|| format!("打开场景反馈失败：{}", path.display()))?;
    file.try_lock()?;
    target.check_opened(&file)?;
    let size = file.metadata()?.len();
    anyhow::ensure!(size.saturating_add(line.len() as u64) <= MAX_FEEDBACK_BYTES,
        "场景反馈已达到 16 MiB 上限。请先归档 .firstcut/scene-feedback.jsonl，再继续记录。");
    file.seek(SeekFrom::End(0))?;
    file.write_all(&line)?;
    file.sync_data()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn feedback_history_keeps_latest_correction() {
        let dir = std::env::temp_dir().join(format!("firstcut_scene_feedback_{}", std::process::id()));
        let path = store_path(&dir);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut record = SceneFeedbackRecord {
            photo_path: "a.jpg".into(), predicted_scene: "portrait".into(),
            predicted_reason: "检测到人脸".into(), selected_scene: "stage".into(),
            note: "舞台演出".into(), recorded_at_unix_ms: 1, faces: 1,
            analysis_mode: Some("ai".into()), scores: None,
        };
        append(&dir, &path, &record).unwrap();
        record.selected_scene = "portrait".into();
        record.recorded_at_unix_ms = 2;
        append(&dir, &path, &record).unwrap();
        let latest = load_latest(&dir, &path).unwrap();
        assert_eq!(latest["a.jpg"].selected_scene, "portrait");
        assert_eq!(fs::read_to_string(&path).unwrap().lines().count(), 2);
        let _ = fs::remove_dir_all(dir);
    }
}
