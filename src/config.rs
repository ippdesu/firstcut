//! 评分权重与指标参数配置（M1 默认值，M5 用真实照片人工校准）

/// 各维度权重（总和应为 1.0）
#[derive(Debug, Clone, Copy)]
pub struct ScoreWeights {
    pub sharpness: f64,
    pub exposure: f64,
    pub noise: f64,
}

impl Default for ScoreWeights {
    fn default() -> Self {
        ScoreWeights {
            sharpness: 0.45,
            exposure: 0.30,
            noise: 0.25,
        }
    }
}

/// 像素指标曲线参数（饱和速度）
#[derive(Debug, Clone, Copy)]
pub struct MetricParams {
    /// 清晰度饱和常数：归一化清晰度 = k 时得 ~63 分
    /// 实测范围（1024px 分析图，33MP 索尼 JPG）：12 万 ~ 172 万
    pub sharpness_k: f64,
    /// 噪点容忍度基准（ISO 100 时）：暗部噪声 = k0 时得 ~37 分
    /// 实测范围：0.5 ~ 10.4
    pub noise_k0: f64,
}

impl Default for MetricParams {
    fn default() -> Self {
        MetricParams {
            sharpness_k: 800_000.0,
            noise_k0: 4.5,
        }
    }
}

/// 连拍去重参数（M2）
#[derive(Debug, Clone, Copy)]
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
