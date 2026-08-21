//! 曝光/直方图分析（M1）

use crate::decode::{AnalyzedImage, OVEREXPOSED_THRESHOLD, UNDEREXPOSED_THRESHOLD};

/// 曝光统计
pub struct ExposureStats {
    /// 过曝像素比例（0-1）
    pub over_ratio: f64,
    /// 欠曝像素比例（0-1）
    pub under_ratio: f64,
    /// 平均亮度（0-255）
    pub mean: f64,
}

pub fn exposure_stats(img: &AnalyzedImage) -> ExposureStats {
    let total = img.width as f64 * img.height as f64;
    if total == 0.0 {
        return ExposureStats { over_ratio: 0.0, under_ratio: 0.0, mean: 0.0 };
    }
    let over: u64 = img.histogram[OVEREXPOSED_THRESHOLD as usize..].iter().sum();
    let under: u64 = img.histogram[..=UNDEREXPOSED_THRESHOLD as usize].iter().sum();
    let mean: f64 = img
        .histogram
        .iter()
        .enumerate()
        .map(|(v, c)| v as f64 * *c as f64)
        .sum::<f64>()
        / total;
    ExposureStats {
        over_ratio: over as f64 / total,
        under_ratio: under as f64 / total,
        mean,
    }
}

/// 曝光分数（0-100，100 最佳）
///
/// 惩罚项：
/// - 死白/死黑像素比例（过曝欠曝直接扣分）
/// - 平均亮度偏离目标亮度（默认 128，暗调/亮调环境可配）程度，用高斯曲线衰减
pub fn exposure_score(stats: &ExposureStats, target: f64) -> f64 {
    let clip_penalty = 4.0 * stats.over_ratio + 4.0 * stats.under_ratio;
    let mean_dev = ((stats.mean - target) / 70.0).powi(2);
    let tone = (-mean_dev).exp();
    (100.0 * (1.0 - clip_penalty).max(0.0) * tone).clamp(0.0, 100.0)
}
