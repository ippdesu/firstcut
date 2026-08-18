//! 图像解码与预处理（M1）
//!
//! 解码 JPG → 下采样到分析尺寸（长边 ≤ 1024px）→ 灰度 + 直方图。
//! RAW 文件（ARW）在此阶段不做解码（Phase 1 约定：JPG 评分、映射到 ARW）。

use std::path::Path;

use anyhow::Result;
use image::GenericImageView;

/// 分析用图像：灰度数据 + 直方图 + RGB 数据（AI 推理用）
pub struct AnalyzedImage {
    pub width: u32,
    pub height: u32,
    /// 灰度像素（长度 = width*height）
    pub luma: Vec<u8>,
    /// RGB 像素（长度 = width*height*3，AI 模型输入用）
    pub rgb: Vec<u8>,
    /// 256 级亮度直方图
    pub histogram: [u64; 256],
    /// 亮度方差（对比度，用于归一化清晰度指标）
    pub luma_variance: f64,
}

/// 分析用长边上限（px）。解码后先缩到该尺寸再做指标计算，吞吐与精度折中。
pub const ANALYSIS_MAX_DIM: u32 = 1024;

/// 解码并预处理照片；非 JPG 或解码失败返回 None（不阻断流水线）
pub fn load_analysis_image(path: &Path) -> Result<Option<AnalyzedImage>> {
    let img = match image::open(path) {
        Ok(img) => img,
        Err(_) => return Ok(None),
    };
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return Ok(None);
    }
    let max_dim = w.max(h);
    let (nw, nh) = if max_dim > ANALYSIS_MAX_DIM {
        let scale = ANALYSIS_MAX_DIM as f32 / max_dim as f32;
        (
            ((w as f32 * scale).round() as u32).max(1),
            ((h as f32 * scale).round() as u32).max(1),
        )
    } else {
        (w, h)
    };

    let small = if (nw, nh) != (w, h) {
        img.resize(nw, nh, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let luma = small.to_luma8();

    let mut histogram = [0u64; 256];
    for &p in luma.as_raw() {
        histogram[p as usize] += 1;
    }

    // 亮度方差（由直方图计算）
    let total = (nw * nh) as f64;
    let mean = histogram
        .iter()
        .enumerate()
        .map(|(v, c)| v as f64 * *c as f64)
        .sum::<f64>()
        / total;
    let luma_variance = histogram
        .iter()
        .enumerate()
        .map(|(v, c)| *c as f64 * (v as f64 - mean).powi(2))
        .sum::<f64>()
        / total;

    Ok(Some(AnalyzedImage {
        width: nw,
        height: nh,
        luma: luma.into_raw(),
        rgb: small.to_rgb8().into_raw(),
        histogram,
        luma_variance,
    }))
}

/// 亮度过曝阈值：≥ 此值视为死白
pub const OVEREXPOSED_THRESHOLD: u8 = 250;
/// 亮度欠曝阈值：≤ 此值视为死黑
pub const UNDEREXPOSED_THRESHOLD: u8 = 5;
