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
        "# ============================================================\n\
         # firstcut 评分配置模板\n\
         # 用法: pic_process score <目录> --config 本文件\n\
         # 可多场景存多份轮着用: 人像.toml / 打鸟.toml / 夜景.toml / 飞机.toml\n\
         # 只写想改的字段即可，未写的字段用默认值\n\
         # ============================================================\n\
         \n\
         # [weights] 五维评分权重（总和应约等于 1.0，偏差超过 0.05 会拒绝加载）\n\
         [weights]\n\
         # 清晰度：主体（人脸/头部）是否合焦。对焦失败是打鸟/飞机/运动的主要废片\n\
         #   原因 → 这类场景调高（0.40~0.45）；人像大光圈虚化背景不会被误判\n\
         #   （主体感知指标），可维持或略降。\n\
         sharpness = {}\n\
         # 曝光：过曝/欠曝比例 + 亮度偏离目标（exposure_target）。白天顺光 → 调高\n\
         #   至 0.25~0.30 让欠曝照片沉底；夜景/暗调创作 → 调低至 0.10~0.15。\n\
         exposure = {}\n\
         # 噪点：暗部噪声 + ISO 容忍度。高 ISO 打鸟/室内 → 调低（更宽容）；\n\
         #   追求画质的低 ISO 场景 → 调高。\n\
         noise = {}\n\
         # 构图：主体在三分法位置与画面占比（基于人脸/人体检测）。人像 → 调高至\n\
         #   0.20~0.25；动物/飞机（无主体检测时给中性分）→ 调低。\n\
         composition = {}\n\
         # 美学：CLIPIQA 主观美感分。风光/人文 → 可调高；记录性内容（翻拍/素材）→ 调低。\n\
         aesthetic = {}\n\
         \n\
         # [metric] 评分曲线与输出参数\n\
         [metric]\n\
         # 清晰度饱和常数：越大对轻微模糊越宽容（分数分布更靠上），越小越严格。\n\
         sharpness_k = {}\n\
         # 噪点容忍基准：越大越宽容噪点（高 ISO 照片分数更高）。\n\
         noise_k0 = {}\n\
         # 曝光理想平均亮度（0-255）：暗调环境（夜景/黄昏/室内）可调低如 90，\n\
         # 亮调环境（雪景/正午海边）可调高如 150。\n\
         exposure_target = {}\n\
         # 星级分档：总分 ≥ 阈值得对应星数（XMP xmp:Rating，Lightroom 可读）\n\
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
