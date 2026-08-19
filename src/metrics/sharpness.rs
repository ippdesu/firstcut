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

/// 强边缘像素占比（Sobel 梯度幅值 > 阈值 的比例，0-1）
///
/// 用于区分"真糊"与"平滑场景"：
/// - 真糊（运动模糊/失焦）：场景本有边缘，边缘占比正常，但梯度被抹低
/// - 平滑场景（天空/虚化背景）：边缘占比极低（<2%），低清晰度分是误杀
pub fn edge_ratio(luma: &[u8], w: u32, h: u32, grad_threshold: f64) -> f64 {
    let w = w as usize;
    let h = h as usize;
    if w < 3 || h < 3 {
        return 0.0;
    }
    let thr2 = grad_threshold * grad_threshold;
    let mut strong = 0u64;
    let mut total = 0u64;
    for y in (1..h - 1).step_by(2) {
        for x in (1..w - 1).step_by(2) {
            let i = |dx: isize, dy: isize| ((y as isize + dy) * w as isize + (x as isize + dx)) as usize;
            let gx = luma[i(1, -1)] as f64 + 2.0 * luma[i(1, 0)] as f64 + luma[i(1, 1)] as f64
                - (luma[i(-1, -1)] as f64 + 2.0 * luma[i(-1, 0)] as f64 + luma[i(-1, 1)] as f64);
            let gy = luma[i(-1, 1)] as f64 + 2.0 * luma[i(0, 1)] as f64 + luma[i(1, 1)] as f64
                - (luma[i(-1, -1)] as f64 + 2.0 * luma[i(0, -1)] as f64 + luma[i(1, -1)] as f64);
            let g2 = gx * gx + gy * gy;
            if g2 > thr2 {
                strong += 1;
            }
            total += 1;
        }
    }
    if total == 0 {
        0.0
    } else {
        strong as f64 / total as f64
    }
}

/// 清晰度分数（0-100，100 最清晰）
///
/// `k` 控制饱和速度：归一化值 = k 时约得 63 分。
pub fn sharpness_score(normalized: f64, k: f64) -> f64 {
    100.0 * (1.0 - (-normalized / k).exp())
}

/// reblur 差分锐度（分块 P90）：原图与 3x3 box blur 两遍后的差异均值
///
/// 这是对"边缘锐利度"的直接度量：
/// - 合焦边缘（窄过渡）：模糊后变化大 → 差异高
/// - 失焦/运动模糊边缘（宽过渡）：模糊后几乎不变 → 差异低
/// - 平滑区域（皮肤/虚化背景）：差异≈0，但不拖累主体块（取 P90）
/// 解决梯度方差把"合焦但内容平滑"的人像误判为糊的问题。
pub fn reblur_block_percentile(luma: &[u8], w: u32, h: u32, block: u32, pct: f64) -> f64 {
    let w = w as usize;
    let h = h as usize;
    if w < 4 || h < 4 {
        return 0.0;
    }
    // 3x3 box blur 两遍（近似高斯）
    let blurred = box_blur_3x3(&box_blur_3x3(luma, w, h), w, h);
    let block = (block as usize).max(4);
    let mut vals: Vec<f64> = Vec::new();
    let mut by = 0usize;
    while by < h {
        let by_end = (by + block).min(h);
        let mut bx = 0usize;
        while bx < w {
            let bx_end = (bx + block).min(w);
            let mut sum = 0f64;
            let mut n = 0f64;
            for y in by..by_end {
                let row = y * w;
                for x in bx..bx_end {
                    let i = row + x;
                    sum += (luma[i] as f64 - blurred[i] as f64).abs();
                    n += 1.0;
                }
            }
            if n > 0.0 {
                vals.push(sum / n);
            }
            bx = bx_end;
        }
        by = by_end;
    }
    if vals.is_empty() {
        return 0.0;
    }
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let idx = (((vals.len() as f64) * pct).floor() as usize).min(vals.len() - 1);
    vals[idx]
}

/// 3x3 box blur（边界复制）
fn box_blur_3x3(luma: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; luma.len()];
    for y in 0..h {
        for x in 0..w {
            let mut sum = 0u64;
            let mut n = 0u64;
            for dy in -1i32..=1 {
                let yy = (y as i32 + dy).clamp(0, h as i32 - 1) as usize;
                for dx in -1i32..=1 {
                    let xx = (x as i32 + dx).clamp(0, w as i32 - 1) as usize;
                    sum += luma[yy * w + xx] as u64;
                    n += 1;
                }
            }
            out[y * w + x] = (sum / n) as u8;
        }
    }
    out
}

/// 指定区域（归一化坐标）内的 reblur 差分均值——"主体区域锐度"。
///
/// 用于人脸检测命中时：评估人脸及周边区域的合焦程度，
/// 不受大面积虚化背景影响（大光圈人像的正确语义）。
pub fn reblur_mean_region(
    luma: &[u8],
    w: u32,
    h: u32,
    cx: f64,
    cy: f64,
    half_w: f64,
    half_h: f64,
) -> f64 {
    let w = w as usize;
    let h = h as usize;
    if w < 8 || h < 8 {
        return 0.0;
    }
    let blurred = box_blur_3x3(&box_blur_3x3(luma, w, h), w, h);
    let x0 = (((cx - half_w) * w as f64).round() as i64).clamp(0, w as i64 - 1) as usize;
    let x1 = (((cx + half_w) * w as f64).round() as i64).clamp(0, w as i64 - 1) as usize;
    let y0 = (((cy - half_h) * h as f64).round() as i64).clamp(0, h as i64 - 1) as usize;
    let y1 = (((cy + half_h) * h as f64).round() as i64).clamp(0, h as i64 - 1) as usize;
    let mut sum = 0f64;
    let mut n = 0f64;
    for y in y0..=y1 {
        let row = y * w;
        for x in x0..=x1 {
            let i = row + x;
            sum += (luma[i] as f64 - blurred[i] as f64).abs();
            n += 1.0;
        }
    }
    if n == 0.0 {
        0.0
    } else {
        sum / n
    }
}

/// 主体区域锐度 → 分数（reblur 均值 4~14 映射 30~95）
pub fn region_sharpness_score(region_reblur: f64) -> f64 {
    let v = region_reblur - 2.5;
    if v <= 0.0 {
        return 20.0;
    }
    (100.0 * (1.0 - (-v / 3.5).exp())).clamp(0.0, 100.0)
}
