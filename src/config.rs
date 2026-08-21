//! 评分配置（M1/M5）：权重、曲线参数、星级分档、曝光目标
//!
//! 支持 `--config <file.toml>` 加载用户配置（多场景可存多份：
//! 人像.toml / 打鸟.toml / 夜景.toml）。未提供时使用内置默认值。

use anyhow::Result;
use serde::Deserialize;
use std::path::Path;

/// 各维度权重（总和应为 1.0）
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct ScoreWeights {
    pub sharpness: f64,
    pub exposure: f64,
    pub noise: f64,
    pub composition: f64,
    pub aesthetic: f64,
}

impl Default for ScoreWeights {
    fn default() -> Self {
        ScoreWeights {
            sharpness: 0.35,
            exposure: 0.20,
            noise: 0.15,
            composition: 0.15,
            aesthetic: 0.15,
        }
    }
}

/// 指标曲线参数 + 星级分档 + 曝光目标
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct MetricParams {
    /// 清晰度饱和常数：归一化清晰度 = k 时得 ~63 分
    pub sharpness_k: f64,
    /// 噪点容忍度基准（ISO 100 时）
    pub noise_k0: f64,
    /// 曝光理想平均亮度（0-255）。暗调环境（夜景/室内）可调低，亮调环境调高
    pub exposure_target: f64,
    /// 星级分档阈值（总分 ≥ 各档位得对应星数）
    pub rating_5: f64,
    pub rating_4: f64,
    pub rating_3: f64,
    pub rating_2: f64,
}

impl Default for MetricParams {
    fn default() -> Self {
        MetricParams {
            sharpness_k: 800_000.0,
            noise_k0: 3.0,
            exposure_target: 128.0,
            rating_5: 75.0,
            rating_4: 60.0,
            rating_3: 45.0,
            rating_2: 30.0,
        }
    }
}

/// 连拍去重参数（M2）
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct DedupParams {
    /// 时间聚类间隔阈值（秒）：间隔 ≤ 此值视为同一连拍组
    pub gap_secs: f64,
    /// dHash 汉明距离阈值：≤ 此值视为同一场景子簇
    pub dhash_threshold: u32,
    /// 每个子簇保留前 K 张
    pub keep_k: usize,
}

impl Default for DedupParams {
    fn default() -> Self {
        DedupParams {
            gap_secs: 2.0,
            dhash_threshold: 10,
            keep_k: 2,
        }
    }
}

/// 完整评分配置
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct ScoreConfig {
    pub weights: ScoreWeights,
    pub metric: MetricParams,
}

impl Default for ScoreConfig {
    fn default() -> Self {
        ScoreConfig {
            weights: ScoreWeights::default(),
            metric: MetricParams::default(),
        }
    }
}

/// 从 TOML 文件加载配置；字段缺失用默认值
pub fn load_config(path: &Path) -> Result<ScoreConfig> {
    let text = std::fs::read_to_string(path)?;
    let cfg: ScoreConfig = toml::from_str(&text)?;
    let w = &cfg.weights;
    let sum = w.sharpness + w.exposure + w.noise + w.composition + w.aesthetic;
    if (sum - 1.0).abs() > 0.05 {
        anyhow::bail!("权重之和应约为 1.0，当前为 {sum:.3}（{path:?}）");
    }
    Ok(cfg)
}

/// 生成默认配置模板文本（供 `pic_process config-template` 输出）
pub fn config_template() -> String {
    let c = ScoreConfig::default();
    format!(
        "# firstcut 评分配置模板（可多场景存多份，如 portrait.toml / bird.toml）\n\
         # 用法: pic_process score <目录> --config portrait.toml\n\
         [weights]\n\
         sharpness = {}\n\
         exposure = {}\n\
         noise = {}\n\
         composition = {}\n\
         aesthetic = {}\n\
         \n\
         [metric]\n\
         # 清晰度饱和常数（越大越宽松）\n\
         sharpness_k = {}\n\
         # 噪点容忍基准（越大越宽容噪点）\n\
         noise_k0 = {}\n\
         # 曝光理想平均亮度（0-255）：夜景/暗调环境可调低如 90\n\
         exposure_target = {}\n\
         # 星级分档（总分 ≥ 阈值得对应星数）\n\
         rating_5 = {}\n\
         rating_4 = {}\n\
         rating_3 = {}\n\
         rating_2 = {}\n",
        c.weights.sharpness,
        c.weights.exposure,
        c.weights.noise,
        c.weights.composition,
        c.weights.aesthetic,
        c.metric.sharpness_k,
        c.metric.noise_k0,
        c.metric.exposure_target,
        c.metric.rating_5,
        c.metric.rating_4,
        c.metric.rating_3,
        c.metric.rating_2,
    )
}
