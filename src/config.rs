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
            // M5 决策 A：曝光权重提高到 0.25（欠曝照片不再虚高），
            // 从清晰度挪 0.05 保持权重和 = 1.0（与 A2 模拟验证一致）
            sharpness: 0.30,
            exposure: 0.25,
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
    /// 曝光理想亮度（0-255 中灰码值）。暗调环境可调低，高调/雪景可调高
    pub exposure_target: f64,
    /// 暗侧满分容差（EV）：0 ~ -此档位内视为正确曝光（默认 1.0）
    pub exposure_ev_full_lo: f64,
    /// 亮侧满分容差（EV）：0 ~ +此档位内视为正确曝光（默认 1.0）
    pub exposure_ev_full_hi: f64,
    /// 暗侧衰减到 0 的档位（默认 4.0；RAW 欠曝可救，故较宽）
    pub exposure_ev_lo: f64,
    /// 亮侧衰减到 0 的档位（默认 2.0；高光溢出不可恢复，故较窄）
    pub exposure_ev_hi: f64,
    /// 主体脸亮度在曝光判定中的权重（0 = 只看全图，1 = 只看主体脸）
    pub exposure_subject_blend: f64,
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
            // M6 决策：曝光判定按 EV 容差带，而不是"偏离中灰多少码值"。
            // ±1 EV（码值 92~176）内满分 —— 这是 AE 的正常波动范围；
            // 暗侧到 -4 EV、亮侧到 +2 EV 线性降为 0（亮侧更陡：
            // 高光溢出在 JPG 里不可恢复，暗部在 RAW 里通常还能救）。
            // 两侧容差独立，因为"允许暗"与"允许亮"不是同一件事。
            // 判定亮度 = 0.5×全图截尾均值 + 0.5×主体脸区域均值（有主体脸时），
            // 修正舞台黑幕布 / 白裙白背景两类"全图均值不代表主体"的误判。
            exposure_target: 128.0,
            exposure_ev_full_lo: 1.0,
            exposure_ev_full_hi: 1.0,
            exposure_ev_lo: 4.0,
            exposure_ev_hi: 2.0,
            exposure_subject_blend: 0.5,
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

/// 评分配置指纹（用于缓存键）
///
/// 评分曲线参数变化后必须让旧缓存失效，否则用户改了 `--config` 却看到
/// 与之前完全一样的结果（缓存命中直接返回旧的五维分数）。
/// 这里把参与评分的全部参数按位哈希成一个整数，存进缓存行一起比对。
pub fn config_fingerprint(cfg: &ScoreConfig) -> i64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut h = DefaultHasher::new();
    let w = &cfg.weights;
    for v in [w.sharpness, w.exposure, w.noise, w.composition, w.aesthetic] {
        v.to_bits().hash(&mut h);
    }
    let m = &cfg.metric;
    for v in [
        m.sharpness_k,
        m.noise_k0,
        m.exposure_target,
        m.exposure_ev_full_lo,
        m.exposure_ev_full_hi,
        m.exposure_ev_lo,
        m.exposure_ev_hi,
        m.exposure_subject_blend,
        m.rating_5,
        m.rating_4,
        m.rating_3,
        m.rating_2,
    ] {
        v.to_bits().hash(&mut h);
    }
    h.finish() as i64
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
    // 曝光容差带必须严格嵌套，否则衰减区间宽度为 0，分数会出现断崖
    let m = &cfg.metric;
    if m.exposure_ev_full_lo < 0.0 || m.exposure_ev_full_hi < 0.0 {
        anyhow::bail!("曝光满分容差不能为负（{path:?}）");
    }
    if m.exposure_ev_lo <= m.exposure_ev_full_lo {
        anyhow::bail!(
            "exposure_ev_lo({}) 必须大于 exposure_ev_full_lo({})（{path:?}）",
            m.exposure_ev_lo,
            m.exposure_ev_full_lo
        );
    }
    if m.exposure_ev_hi <= m.exposure_ev_full_hi {
        anyhow::bail!(
            "exposure_ev_hi({}) 必须大于 exposure_ev_full_hi({})（{path:?}）",
            m.exposure_ev_hi,
            m.exposure_ev_full_hi
        );
    }
    if !(0.0..=1.0).contains(&m.exposure_subject_blend) {
        anyhow::bail!("exposure_subject_blend 应在 0~1 之间（{path:?}）");
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
         # 可多场景存多份轮着用: 人像.toml / 舞台.toml / 夜景.toml / 打鸟.toml\n\
         # 内置场景预设可直接生成: pic_process config-template --preset stage\n\
         # 只写想改的字段即可，未写的字段用默认值\n\
         # ============================================================\n\
         \n\
         # [weights] 五维评分权重（总和应约等于 1.0，偏差超过 0.05 会拒绝加载）\n\
         [weights]\n\
         # 清晰度：主体（人脸/头部）是否合焦。对焦失败是打鸟/飞机/运动的主要废片\n\
         #   原因 → 这类场景调高（0.40~0.45）；人像大光圈虚化背景不会被误判\n\
         #   （主体感知指标），可维持或略降。\n\
         sharpness = {}\n\
         # 曝光：过曝/欠曝比例 + 判定亮度偏离目标（见下方 EV 容差带）。白天顺光 →\n\
         #   调高至 0.25~0.30 让欠曝照片沉底；夜景/暗调创作 → 调低至 0.10~0.15。\n\
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
         # --- 曝光：按曝光档位（EV）判定，与相机/场景无关 ---\n\
         # 理想亮度（0-255 中灰码值）：夜景/暗调可调低如 110，雪景/高调可调高如 150。\n\
         exposure_target = {}\n\
         # 暗侧满分容差（EV）：0 ~ -此档位内视为正确曝光。AE 正常波动约 ±1 档，\n\
         #   舞台/夜景主体常低 1~2 档，可放宽到 1.5~2.0。\n\
         exposure_ev_full_lo = {}\n\
         # 亮侧满分容差（EV）：白背景/雪景/白裙可放宽到 1.5~2.0。\n\
         exposure_ev_full_hi = {}\n\
         # 暗侧衰减到 0 的档位（须大于 exposure_ev_full_lo）：欠曝在 RAW 里\n\
         #   通常可救，默认 4.0（-4 档得 0 分）。\n\
         exposure_ev_lo = {}\n\
         # 亮侧衰减到 0 的档位（须大于 exposure_ev_full_hi）：高光溢出不可恢复，\n\
         #   默认 2.0（+2 档得 0 分）。\n\
         exposure_ev_hi = {}\n\
         # 主体脸亮度权重（0~1）：有主体级人脸（高度 ≥ 4%）时，取最亮的一张\n\
         #   主体脸区域均值，把判定亮度往中灰方向拉（夹在「全图 ~ 中灰」之间，\n\
         #   不越过中灰），再按此权重混合。设为 0 则只看全图（适合无主体人脸的题材）。\n\
         exposure_subject_blend = {}\n\
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
        c.metric.exposure_ev_full_lo,
        c.metric.exposure_ev_full_hi,
        c.metric.exposure_ev_lo,
        c.metric.exposure_ev_hi,
        c.metric.exposure_subject_blend,
        c.metric.rating_5,
        c.metric.rating_4,
        c.metric.rating_3,
        c.metric.rating_2,
    )
}

/// 内置场景预设（编译进二进制，`config-template --preset <名>` 输出）
pub const PRESETS: &[(&str, &str)] = &[
    ("portrait", include_str!("../presets/portrait.toml")),
    ("stage", include_str!("../presets/stage.toml")),
    ("highkey", include_str!("../presets/highkey.toml")),
    ("sports", include_str!("../presets/sports.toml")),
    ("lowlight", include_str!("../presets/lowlight.toml")),
];

/// 按名字取场景预设文本
pub fn preset(name: &str) -> Option<&'static str> {
    PRESETS.iter().find(|(n, _)| *n == name).map(|(_, t)| *t)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个内置场景预设都必须能通过 load_config 的校验（权重和、字段类型）
    #[test]
    fn all_presets_are_loadable() {
        let dir = std::env::temp_dir().join("firstcut_preset_test");
        std::fs::create_dir_all(&dir).unwrap();
        for (name, text) in PRESETS {
            let path = dir.join(format!("{name}.toml"));
            std::fs::write(&path, text).unwrap();
            let cfg = load_config(&path).unwrap_or_else(|e| panic!("预设 {name} 加载失败: {e:#}"));
            let w = &cfg.weights;
            let sum = w.sharpness + w.exposure + w.noise + w.composition + w.aesthetic;
            assert!((sum - 1.0).abs() < 0.05, "预设 {name} 权重和 {sum}");
            assert!(cfg.metric.exposure_ev_lo > cfg.metric.exposure_ev_full_lo);
            assert!(cfg.metric.exposure_ev_hi > cfg.metric.exposure_ev_full_hi);
            assert!((0.0..=1.0).contains(&cfg.metric.exposure_subject_blend));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 未知预设名必须返回 None（CLI 据此报错并列出可用名）
    #[test]
    fn unknown_preset_is_none() {
        assert!(preset("nope").is_none());
        assert!(preset("stage").is_some());
    }
}
