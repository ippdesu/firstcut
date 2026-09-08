//! 评分配置（M1/M5）：权重、曲线参数、星级分档、曝光目标
//!
//! 支持 `--config <file.toml>` 加载用户配置（多场景可存多份：
//! 人像.toml / 打鸟.toml / 夜景.toml）。未提供时使用内置默认值。

use anyhow::Result;
use serde::Deserialize;
use std::path::Path;

/// 各维度权重（总和应为 1.0）
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
    /// 星级分档模式："relative"（批次内相对排名，默认）或 "absolute"（总分阈值）
    pub star_mode: String,
    /// relative 模式：5 星分界（批次内百分位，0 = 最好）
    pub star_five_pct: f64,
    /// relative 模式：4 星分界
    pub star_four_pct: f64,
    /// relative 模式：3 星分界
    pub star_three_pct: f64,
    /// relative 模式：2 星分界（其余为 1 星）
    pub star_two_pct: f64,
    /// absolute 模式：总分 ≥ 各档位得对应星数
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
            // M7 决策：星级默认按批次内相对排名（分数绝对值仍写进 CSV/XMP，
            // 但星级保证每批都有区分度——实测绝对阈值下 119 张全落在 4~5 星）
            star_mode: "relative".to_string(),
            star_five_pct: 10.0,
            star_four_pct: 30.0,
            star_three_pct: 65.0,
            star_two_pct: 90.0,
            rating_5: 75.0,
            rating_4: 60.0,
            rating_3: 45.0,
            rating_2: 30.0,
        }
    }
}

/// 连拍去重参数（M2/M9）
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DedupParams {
    /// 时间聚类间隔阈值（秒）：间隔 ≤ 此值视为同一连拍组
    pub gap_secs: f64,
    /// dHash 汉明距离阈值：≤ 此值视为同一场景子簇
    pub dhash_threshold: u32,
    /// 每个**保留单元**（M9 启用时为姿态簇，否则为 dHash 子簇）内保留前 K 张
    pub keep_k: usize,
    /// M9：按 SCRFD 关键点姿态描述子自适应保留（在 dHash 子簇内再按姿势分簇，
    /// 每个姿势簇各自保留 keep_k 张；false 回退为按 dHash 子簇保留）
    pub adaptive_keep: bool,
    /// M9：姿态聚类阈值——描述子（关键点按人脸框归一化的 10 维向量）
    /// 欧氏距离 > 此值判为不同姿势。定标实测（M9，棚拍 18 帧同主体连拍）：
    /// 同姿势两两距离 0.02~0.06、跨姿势 0.46~0.51，双峰清晰，0.25 居中；
    /// 舞台（萤火虫）连拍姿态连续变化时无真空带，0.25 给出"瞬间"粒度。
    pub pose_cluster_threshold: f64,
    /// M9：单个连拍组的保留总量上限（姿态簇多时防止 90 帧连拍留几十张；
    /// 超出按总分从高到低截断）
    pub burst_group_cap: usize,
}

impl Default for DedupParams {
    fn default() -> Self {
        DedupParams {
            gap_secs: 2.0,
            dhash_threshold: 10,
            // M9 决策：姿态簇内保留 3 张（用户定；表情成功率低的连拍留足备选）
            keep_k: 3,
            adaptive_keep: true,
            pose_cluster_threshold: 0.25,
            burst_group_cap: 20,
        }
    }
}

/// 完整评分配置
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScoreConfig {
    pub weights: ScoreWeights,
    pub metric: MetricParams,
    pub dedup: DedupParams,
}

impl Default for ScoreConfig {
    fn default() -> Self {
        ScoreConfig {
            weights: ScoreWeights::default(),
            metric: MetricParams::default(),
            dedup: DedupParams::default(),
        }
    }
}

/// CLI `-k` 与 `[dedup]` 配置的合成：`-k` **显式给出**时才覆盖 `keep_k`，
/// 其余参数一律来自配置。`score` 与 `review` 都走这里——
/// 此前 `-k` 带 CLI 默认值会把配置里的 keep_k 静默覆盖成死配置
/// （V2-2，M6-4 同类："配置写了但不生效"）。
pub fn effective_dedup(keep: Option<usize>, cfg: &ScoreConfig) -> DedupParams {
    match keep {
        Some(k) => DedupParams { keep_k: k, ..cfg.dedup },
        None => cfg.dedup,
    }
}

/// 评分配置指纹（用于缓存键）
///
/// **只覆盖影响缓存值的参数**：缓存行存的是五维子分 + dHash + 人脸数，
/// 只有这些曲线参数会改变子分。权重与星级阈值只在运行期合成总分/星级时
/// 使用，改它们不应该触发全量解码+AI 重算（那是用户最高频的调参动作）。
pub fn config_fingerprint(cfg: &ScoreConfig) -> i64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut h = DefaultHasher::new();
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
    // 星级模式必须显式合法：拼错会静默走 relative，用户以为改成了 absolute
    if !(m.star_mode.eq_ignore_ascii_case("relative") || m.star_mode.eq_ignore_ascii_case("absolute"))
    {
        anyhow::bail!(
            "star_mode 只能是 \"relative\" 或 \"absolute\"，当前为 {:?}（{path:?}）",
            m.star_mode
        );
    }
    // 相对分档的百分位必须单调递增，否则会给整批同一星级
    if !(m.star_five_pct <= m.star_four_pct
        && m.star_four_pct <= m.star_three_pct
        && m.star_three_pct <= m.star_two_pct)
    {
        anyhow::bail!(
            "星级百分位必须递增（star_five_pct ≤ star_four_pct ≤ star_three_pct ≤ star_two_pct），当前为 {}/{}/{}/{}（{path:?}）",
            m.star_five_pct,
            m.star_four_pct,
            m.star_three_pct,
            m.star_two_pct
        );
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
         # 星级分档：\n\
         #   mode = 「relative」（默认）按**本次批次的相对排名**给星，保证每批都有区分度；\n\
         #   mode = 「absolute」用下面的总分阈值（跨批次可比，但实测一批照片容易全落在 4~5 星）。\n\
         # relative 模式的百分位分界（0 = 最好；同分并列取平均位次，不会被拆开）\n\
         star_mode = \"{}\"\n\
         star_five_pct = {}\n\
         star_four_pct = {}\n\
         star_three_pct = {}\n\
         star_two_pct = {}\n\
         # absolute 模式的总分阈值（总分 ≥ 阈值得对应星数）\n\
         rating_5 = {}\n\
         rating_4 = {}\n\
         rating_3 = {}\n\
         rating_2 = {}\n\
         \n\
         # [dedup] 连拍去重（M9；只写想改的字段）\n\
         # [dedup]\n\
         # 时间间隔阈值（秒）：≤ 此值视为同一连拍组\n\
         # gap_secs = {}\n\
         # dHash 汉明距离阈值：≤ 此值视为同一场景子簇\n\
         # dhash_threshold = {}\n\
         # 每个保留单元内保留几张（M9 启用时保留单元 = 姿态簇）\n\
         # keep_k = {}\n\
         # M9 自适应保留：按头部姿态描述子在 dHash 子簇内再分姿势簇，\n\
         #   每个姿势簇各自保留 keep_k 张；false 回退旧行为（按 dHash 子簇保留）\n\
         # adaptive_keep = {}\n\
         # 姿态聚类阈值（欧氏距离，> 此值判为不同姿势）：调小分簇更细、\n\
         #   保留更多；调大更宽容、接近旧行为\n\
         # pose_cluster_threshold = {}\n\
         # 单个连拍组保留总量上限（超出按总分截断），防止长连拍留几十张\n\
         # burst_group_cap = {}\n",
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
        c.metric.star_mode,
        c.metric.star_five_pct,
        c.metric.star_four_pct,
        c.metric.star_three_pct,
        c.metric.star_two_pct,
        c.metric.rating_5,
        c.metric.rating_4,
        c.metric.rating_3,
        c.metric.rating_2,
        c.dedup.gap_secs,
        c.dedup.dhash_threshold,
        c.dedup.keep_k,
        c.dedup.adaptive_keep,
        c.dedup.pose_cluster_threshold,
        c.dedup.burst_group_cap,
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

    fn load_text(name: &str, text: &str) -> anyhow::Result<ScoreConfig> {
        let dir = std::env::temp_dir().join("firstcut_cfg_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.toml"));
        std::fs::write(&path, text).unwrap();
        load_config(&path)
    }

    /// 未知字段必须报错：手滑的字段名静默忽略会让用户以为参数已生效
    #[test]
    fn unknown_field_is_rejected() {
        let err = load_text("unknown", "[metric]\nexposure_ev_Io = 2.0\n").unwrap_err();
        assert!(format!("{err:#}").contains("exposure_ev_Io"), "{err:#}");
    }

    /// star_mode 拼错必须报错，不能静默回退 relative
    #[test]
    fn bad_star_mode_is_rejected() {
        let err = load_text("starmode", "[metric]\nstar_mode = \"absolue\"\n").unwrap_err();
        assert!(format!("{err:#}").contains("star_mode"), "{err:#}");
    }

    /// 百分位倒序必须报错（否则整批同星级）
    #[test]
    fn reversed_percentiles_are_rejected() {
        let err = load_text(
            "pct",
            "[metric]\nstar_five_pct = 90.0\nstar_two_pct = 10.0\n",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("递增"), "{err:#}");
    }

    /// 权重和偏离 1.0 过多必须报错
    #[test]
    fn bad_weight_sum_is_rejected() {
        let err = load_text("weights", "[weights]\nsharpness = 0.9\n").unwrap_err();
        assert!(format!("{err:#}").contains("权重"), "{err:#}");
    }

    /// 合法的最小配置可以加载
    #[test]
    fn minimal_config_loads() {
        let cfg = load_text("min", "[metric]\nexposure_target = 120.0\n").unwrap();
        assert_eq!(cfg.metric.exposure_target, 120.0);
        assert_eq!(cfg.weights.sharpness, 0.30);
    }

    /// 配置指纹只覆盖影响缓存值的曲线参数：
    /// 改权重/星级阈值不应触发全量重算，改曲线参数必须触发
    #[test]
    fn fingerprint_covers_only_cached_inputs() {        let base = ScoreConfig::default();
        let fp = config_fingerprint(&base);

        let mut w = ScoreConfig::default();
        w.weights.sharpness = 0.40;
        w.weights.composition = 0.05;
        assert_eq!(config_fingerprint(&w), fp, "改权重不应改变指纹");

        let mut r = ScoreConfig::default();
        r.metric.rating_5 = 80.0;
        assert_eq!(config_fingerprint(&r), fp, "改星级阈值不应改变指纹");

        for mut m in [ScoreConfig::default(), ScoreConfig::default()] {
            m.metric.sharpness_k += 1.0;
            assert_ne!(config_fingerprint(&m), fp, "改 sharpness_k 应改变指纹");
        }
        let mut e = ScoreConfig::default();
        e.metric.exposure_target = 130.0;
        assert_ne!(config_fingerprint(&e), fp, "改曝光目标应改变指纹");
    }

    /// -k 与 [dedup] 的合成：显式 -k 覆盖 keep_k，缺省用配置（V2-2 死配置修复）
    #[test]
    fn effective_dedup_merges_cli_and_config() {
        let mut cfg = ScoreConfig::default();
        cfg.dedup.keep_k = 1;
        cfg.dedup.burst_group_cap = 7;

        // 缺省 -k：完全用配置（保持 cap 等其余字段）
        let d = effective_dedup(None, &cfg);
        assert_eq!(d.keep_k, 1, "配置的 keep_k 必须生效（不再被 CLI 默认值覆盖）");
        assert_eq!(d.burst_group_cap, 7);

        // 显式 -k：只覆盖 keep_k
        let d = effective_dedup(Some(5), &cfg);
        assert_eq!(d.keep_k, 5);
        assert_eq!(d.burst_group_cap, 7, "其余字段仍来自配置");

        // review 路径（直接用配置）与 score 缺省路径结果一致
        assert_eq!(effective_dedup(None, &cfg), cfg.dedup);
    }
}
