//! 清晰度/合焦检测：Tenengrad（Sobel 梯度方差）
//!
//! 模糊图的梯度方差显著小于清晰图。方差对场景纹理敏感（纹理多的图天然高），
//! 因此用饱和曲线映射到 0-100，避免绝对阈值。

use crate::decode::AnalyzedImage;

/// Sobel 梯度方差（Tenengrad），值越大越清晰
pub fn tenengrad_variance(img: &AnalyzedImage) -> f64 {
    let luma = &img.luma;
    let w = img.width as usize;
    let h = img.height as usize;
    if w < 3 || h < 3 {
        return 0.0;
    }

    let mut g_sum = 0f64;
    let mut g2_sum = 0f64;
    let mut n = 0f64;

    // 只采样网格点，加速大图（每 2px 采样一次足够稳定）
    for y in (1..h - 1).step_by(2) {
        for x in (1..w - 1).step_by(2) {
            let i = |dx: isize, dy: isize| ((y as isize + dy) * w as isize + (x as isize + dx)) as usize;
            let gx = luma[i(1, -1)] as f64 + 2.0 * luma[i(1, 0)] as f64 + luma[i(1, 1)] as f64
                - (luma[i(-1, -1)] as f64 + 2.0 * luma[i(-1, 0)] as f64 + luma[i(-1, 1)] as f64);
            let gy = luma[i(-1, 1)] as f64 + 2.0 * luma[i(0, 1)] as f64 + luma[i(1, 1)] as f64
                - (luma[i(-1, -1)] as f64 + 2.0 * luma[i(0, -1)] as f64 + luma[i(1, -1)] as f64);
            let g = gx * gx + gy * gy;
            g_sum += g;
            g2_sum += g * g;
            n += 1.0;
        }
    }
    if n == 0.0 {
        return 0.0;
    }
    let mean = g_sum / n;
    let var = (g2_sum / n - mean * mean).max(0.0);
    var
}

/// 归一化清晰度指标：Tenengrad 方差 ÷ 图像对比度方差
///
/// 除以亮度方差可消除场景纹理/亮度差异的影响（平滑的天空和纹理丰富的
/// 场景也能公平比较），对焦外虚化/运动模糊更敏感。
/// 对比度极低（纯色/无细节）的图像视为不清晰。
pub fn normalized_sharpness(var: f64, luma_variance: f64) -> f64 {
    if luma_variance < 1.0 {
        return 0.0;
    }
    var / luma_variance
}

/// 清晰度分数（0-100，100 最清晰）
///
/// `k` 控制饱和速度：归一化值 = k 时约得 63 分。
pub fn sharpness_score(normalized: f64, k: f64) -> f64 {
    100.0 * (1.0 - (-normalized / k).exp())
}
