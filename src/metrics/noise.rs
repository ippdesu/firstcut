//! 噪点分析（M1）
//!
//! 思路：暗部区域（亮度 < 40）的局部标准差反映真实噪声水平——
//! 亮部差异主要来自场景细节，暗部则更接近传感器噪声。
//! 结合 EXIF ISO 设定容忍度：ISO 越高，同样噪声越可接受。

use crate::decode::AnalyzedImage;

/// 暗部判定阈值（亮度 < 此值视为暗部）
const DARK_THRESHOLD: u8 = 40;
/// 局部标准差计算的块大小
const BLOCK: usize = 8;

/// 暗部噪声度量：暗部 8x8 块的标准差中位数（0-~100）
pub fn dark_noise_metric(img: &AnalyzedImage) -> f64 {
    let luma = &img.luma;
    let w = img.width as usize;
    let h = img.height as usize;
    if w < BLOCK || h < BLOCK {
        return 0.0;
    }

    let mut block_stds: Vec<f64> = Vec::new();
    let mut by = 0;
    while by + BLOCK <= h {
        let mut bx = 0;
        while bx + BLOCK <= w {
            let mut sum = 0f64;
            let mut dark = 0usize;
            for y in by..by + BLOCK {
                let row = y * w + bx;
                for x in 0..BLOCK {
                    let v = luma[row + x];
                    sum += v as f64;
                    if v < DARK_THRESHOLD {
                        dark += 1;
                    }
                }
            }
            // 只统计暗块（块内过半像素为暗部）
            if dark * 2 >= BLOCK * BLOCK {
                let n = (BLOCK * BLOCK) as f64;
                let mean = sum / n;
                let mut var = 0f64;
                for y in by..by + BLOCK {
                    let row = y * w + bx;
                    for x in 0..BLOCK {
                        let d = luma[row + x] as f64 - mean;
                        var += d * d;
                    }
                }
                block_stds.push((var / n).sqrt());
            }
            bx += BLOCK;
        }
        by += BLOCK;
    }

    if block_stds.is_empty() {
        // 无暗块（如过曝图）：退回全局暗像素标准差
        let dark_px: Vec<f64> = luma
            .iter()
            .filter(|&&v| v < DARK_THRESHOLD)
            .map(|&v| v as f64)
            .collect();
        if dark_px.is_empty() {
            return 0.0;
        }
        let n = dark_px.len() as f64;
        let mean = dark_px.iter().sum::<f64>() / n;
        (dark_px.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n).sqrt()
    } else {
        // 取低百分位（P15）：最平滑的暗块反映传感器噪声，
        // 避免暗部场景纹理（树叶/深色衣物等）污染指标
        block_stds.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n = block_stds.len();
        let idx = ((n as f64) * 0.15) as usize;
        block_stds[idx.min(n - 1)]
    }
}

/// 噪点分数（0-100，100 最干净）
///
/// `iso` 来自 EXIF（缺失时按 100 计）。容忍度随 ISO 升高而放宽：
/// k = K0 * (1 + 0.3 * log10(iso/100))
pub fn noise_score(metric: f64, iso: u32, k0: f64) -> f64 {
    let iso = iso.max(100);
    let k = k0 * (1.0 + 0.3 * (iso as f64 / 100.0).log10());
    (100.0 * (-metric / k).exp()).clamp(0.0, 100.0)
}
