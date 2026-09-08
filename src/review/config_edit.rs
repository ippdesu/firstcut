//! 配置读取/编辑（M-UI2）：`toml_edit` 保留注释与字段顺序
//!
//! 编辑目标是 review 启动时 `--config` 指定的文件；未指定时保存到
//! `firstcut.toml`（并在 UI 提示需 `--config` 指定后才生效于 score）。
//! 文件不存在时先落一份带完整注释的模板，再做数值替换。

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::ScoreConfig;

/// UI 可编辑的配置值（平铺，便于表单双向绑定）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigValues {
    pub w_sharpness: f64,
    pub w_exposure: f64,
    pub w_noise: f64,
    pub w_composition: f64,
    pub w_aesthetic: f64,
    pub star_mode: String,
    pub star_five_pct: f64,
    pub star_four_pct: f64,
    pub star_three_pct: f64,
    pub star_two_pct: f64,
    pub rating_5: f64,
    pub rating_4: f64,
    pub rating_3: f64,
    pub rating_2: f64,
    pub exposure_target: f64,
    pub exposure_ev_full_lo: f64,
    pub exposure_ev_full_hi: f64,
    pub exposure_ev_lo: f64,
    pub exposure_ev_hi: f64,
    pub exposure_subject_blend: f64,
    pub dedup_keep_k: f64,
    pub dedup_adaptive_keep: bool,
    pub dedup_pose_cluster_threshold: f64,
    pub dedup_burst_group_cap: f64,
}

impl ConfigValues {
    fn from_cfg(c: &ScoreConfig) -> Self {
        ConfigValues {
            w_sharpness: c.weights.sharpness,
            w_exposure: c.weights.exposure,
            w_noise: c.weights.noise,
            w_composition: c.weights.composition,
            w_aesthetic: c.weights.aesthetic,
            star_mode: c.metric.star_mode.clone(),
            star_five_pct: c.metric.star_five_pct,
            star_four_pct: c.metric.star_four_pct,
            star_three_pct: c.metric.star_three_pct,
            star_two_pct: c.metric.star_two_pct,
            rating_5: c.metric.rating_5,
            rating_4: c.metric.rating_4,
            rating_3: c.metric.rating_3,
            rating_2: c.metric.rating_2,
            exposure_target: c.metric.exposure_target,
            exposure_ev_full_lo: c.metric.exposure_ev_full_lo,
            exposure_ev_full_hi: c.metric.exposure_ev_full_hi,
            exposure_ev_lo: c.metric.exposure_ev_lo,
            exposure_ev_hi: c.metric.exposure_ev_hi,
            exposure_subject_blend: c.metric.exposure_subject_blend,
            dedup_keep_k: c.dedup.keep_k as f64,
            dedup_adaptive_keep: c.dedup.adaptive_keep,
            dedup_pose_cluster_threshold: c.dedup.pose_cluster_threshold,
            dedup_burst_group_cap: c.dedup.burst_group_cap as f64,
        }
    }

}

/// 读取当前配置值：文件存在用文件，否则用内置默认
pub fn load(path: &Path) -> Result<ConfigValues> {
    if path.exists() {
        let cfg = crate::config::load_config(path)?;
        Ok(ConfigValues::from_cfg(&cfg))
    } else {
        Ok(ConfigValues::from_cfg(&ScoreConfig::default()))
    }
}

/// 保存配置值：不存在先落模板（带注释），再以 toml_edit 做数值替换（保留注释），
/// 最后用 `load_config` 全量校验写回内容合法才落盘。
pub fn save(path: &Path, values: &ConfigValues) -> Result<()> {
    if !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, crate::config::config_template())?;
    }
    let text = std::fs::read_to_string(path)?;
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("解析配置失败（{}）", path.display()))?;

    // 目标表不存在则创建（模板里 [dedup] 段是注释掉的）
    for t in ["weights", "metric", "dedup"] {
        if doc[t].as_table_mut().is_none() {
            doc[t] = toml_edit::table();
        }
    }

    // (key, value) 对：与 ConfigValues 字段一一对应
    let mut set_f64 = |doc: &mut toml_edit::DocumentMut, table: &str, key: &str, v: f64| {
        if let Some(t) = doc[table].as_table_mut() {
            t[key] = toml_edit::value(v);
        }
    };
    set_f64(&mut doc, "weights", "sharpness", values.w_sharpness);
    set_f64(&mut doc, "weights", "exposure", values.w_exposure);
    set_f64(&mut doc, "weights", "noise", values.w_noise);
    set_f64(&mut doc, "weights", "composition", values.w_composition);
    set_f64(&mut doc, "weights", "aesthetic", values.w_aesthetic);
    if let Some(t) = doc["metric"].as_table_mut() {
        t["star_mode"] = toml_edit::value(values.star_mode.as_str());
        t["star_five_pct"] = toml_edit::value(values.star_five_pct);
        t["star_four_pct"] = toml_edit::value(values.star_four_pct);
        t["star_three_pct"] = toml_edit::value(values.star_three_pct);
        t["star_two_pct"] = toml_edit::value(values.star_two_pct);
        t["rating_5"] = toml_edit::value(values.rating_5);
        t["rating_4"] = toml_edit::value(values.rating_4);
        t["rating_3"] = toml_edit::value(values.rating_3);
        t["rating_2"] = toml_edit::value(values.rating_2);
        t["exposure_target"] = toml_edit::value(values.exposure_target);
        t["exposure_ev_full_lo"] = toml_edit::value(values.exposure_ev_full_lo);
        t["exposure_ev_full_hi"] = toml_edit::value(values.exposure_ev_full_hi);
        t["exposure_ev_lo"] = toml_edit::value(values.exposure_ev_lo);
        t["exposure_ev_hi"] = toml_edit::value(values.exposure_ev_hi);
        t["exposure_subject_blend"] = toml_edit::value(values.exposure_subject_blend);
    }
    if let Some(t) = doc["dedup"].as_table_mut() {
        t["keep_k"] = toml_edit::value(values.dedup_keep_k.max(0.0) as i64);
        t["adaptive_keep"] = toml_edit::value(values.dedup_adaptive_keep);
        t["pose_cluster_threshold"] = toml_edit::value(values.dedup_pose_cluster_threshold);
        t["burst_group_cap"] = toml_edit::value(values.dedup_burst_group_cap.max(0.0) as i64);
    }

    let new_text = doc.to_string();
    // 写回前校验：非法配置不落盘（权重和/容差嵌套/star_mode 等）
    crate::config::load_config_text(&new_text, &path.display().to_string())?;

    // 原子替换：先写临时文件再改名
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, new_text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_preserves_comments_and_roundtrips() {
        let dir = std::env::temp_dir().join("firstcut_cfg_edit_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("c.toml");
        std::fs::write(&path, crate::config::config_template()).unwrap();

        let mut v = load(&path).unwrap();
        v.w_exposure = 0.31;
        v.w_sharpness = 0.24; // 保持权重和 = 1.0（0.24+0.31+0.15×3）
        v.star_mode = "absolute".into();
        v.rating_5 = 80.0;
        v.dedup_keep_k = 5.0;
        save(&path, &v).unwrap();

        // 注释保留（toml_edit 的核心价值）
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# firstcut 评分配置模板"), "头部注释应保留");
        // 数值生效
        let after = load(&path).unwrap();
        assert_eq!(after.w_exposure, 0.31);
        assert_eq!(after.w_sharpness, 0.24);
        assert_eq!(after.star_mode, "absolute");
        assert_eq!(after.rating_5, 80.0);
        assert_eq!(after.dedup_keep_k, 5.0);
        // 合法性（load_config 通过 = save 内部校验已过）
        crate::config::load_config(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_rejects_invalid_values() {
        let dir = std::env::temp_dir().join("firstcut_cfg_edit_invalid");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("c.toml");
        std::fs::write(&path, crate::config::config_template()).unwrap();

        // 权重和越界 → 拒绝且不落盘
        let mut v = load(&path).unwrap();
        v.w_sharpness = 0.9;
        assert!(save(&path, &v).is_err(), "权重和 1.25 应被拒绝");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("sharpness = 0.3"), "非法编辑不应写盘");

        // star_mode 非法 → 拒绝
        let mut v = load(&path).unwrap();
        v.star_mode = "absolue".into();
        assert!(save(&path, &v).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
