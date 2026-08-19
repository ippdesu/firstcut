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
///
/// JPEG 走 zune-jpeg DCT 缩放快速路径（33MP 全解码 ~75ms → DCT 1/8 直读 ~15ms），
/// 其他格式回退 image crate 全解码。
pub fn load_analysis_image(path: &Path) -> Result<Option<AnalyzedImage>> {
    // 先看文件头是否为 JPEG（避免大 ARW 被整体读入）
    let mut head = [0u8; 4];
    let is_jpeg = match std::fs::File::open(path) {
        Ok(mut f) => {
            use std::io::Read;
            f.read_exact(&mut head).is_ok() && head == [0xFF, 0xD8, 0xFF, 0xDB]
                || (head[0] == 0xFF && head[1] == 0xD8 && head[2] == 0xFF)
        }
        Err(_) => false,
    };

    if is_jpeg {
        if let Ok(bytes) = std::fs::read(path) {
            if let Some(img) = decode_jpeg_scaled(&bytes) {
                return Ok(Some(img));
            }
        }
    }

    // 兜底：image crate 全解码
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
    Ok(Some(build_analysis_image(small.to_rgb8())))
}

/// JPEG 快速解码：jpeg-decoder 全解码 + box 降采样（块平均）
///
/// 33MP 全解码 ~400ms；Triangle 滤波缩放 33MP 是另一大开销（~600ms），
/// box 降采样用 8x8 块平均直接到 ~1MP（~50ms），且无振铃、对像素指标更稳。
fn decode_jpeg_scaled(bytes: &[u8]) -> Option<AnalyzedImage> {
    let mut decoder = jpeg_decoder::Decoder::new(bytes);
    let pixels = decoder.decode().ok()?;
    let info = decoder.info()?;
    let (w, h) = (info.width as u32, info.height as u32);
    if w == 0 || h == 0 {
        return None;
    }
    let rgb = pixels;
    if w.max(h) <= ANALYSIS_MAX_DIM {
        return Some(build_analysis_image(image::RgbImage::from_raw(w, h, rgb)?));
    }
    // box 降采样：步长 = ceil(长边/1024)
    let step = (w.max(h) as f32 / ANALYSIS_MAX_DIM as f32).ceil() as u32;
    let (nw, nh, out) = box_downsample(&rgb, w, h, step);
    // 极少情况下（非整数倍）仍略超 1024，用 image crate 收尾
    if nw.max(nh) > ANALYSIS_MAX_DIM {
        let img = image::RgbImage::from_raw(nw, nh, out)?;
        let scale = ANALYSIS_MAX_DIM as f32 / nw.max(nh) as f32;
        let rw = ((nw as f32 * scale).round() as u32).max(1);
        let rh = ((nh as f32 * scale).round() as u32).max(1);
        let resized = image::imageops::resize(&img, rw, rh, image::imageops::FilterType::Triangle);
        return Some(build_analysis_image(resized));
    }
    Some(build_analysis_image(image::RgbImage::from_raw(nw, nh, out)?))
}

/// 块平均降采样：每 step×step 块取均值
fn box_downsample(rgb: &[u8], w: u32, h: u32, step: u32) -> (u32, u32, Vec<u8>) {
    let nw = w / step;
    let nh = h / step;
    let mut out = Vec::with_capacity((nw * nh * 3) as usize);
    let area = (step * step) as u64;
    for oy in 0..nh {
        for ox in 0..nw {
            let mut s = [0u64; 3];
            let y0 = oy * step;
            let x0 = ox * step;
            for dy in 0..step {
                let row = ((y0 + dy) * w + x0) as usize * 3;
                for dx in 0..step {
                    let idx = row + dx as usize * 3;
                    s[0] += rgb[idx] as u64;
                    s[1] += rgb[idx + 1] as u64;
                    s[2] += rgb[idx + 2] as u64;
                }
            }
            out.push((s[0] / area) as u8);
            out.push((s[1] / area) as u8);
            out.push((s[2] / area) as u8);
        }
    }
    (nw, nh, out)
}

fn build_analysis_image(rgb: image::RgbImage) -> AnalyzedImage {
    let (nw, nh) = rgb.dimensions();
    let luma: Vec<u8> = rgb
        .pixels()
        .map(|p| ((p[0] as u32 * 299 + p[1] as u32 * 587 + p[2] as u32 * 114) / 1000) as u8)
        .collect();
    let mut histogram = [0u64; 256];
    for &p in &luma {
        histogram[p as usize] += 1;
    }
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
    AnalyzedImage {
        width: nw,
        height: nh,
        luma,
        rgb: rgb.into_raw(),
        histogram,
        luma_variance,
    }
}

/// 亮度过曝阈值：≥ 此值视为死白
pub const OVEREXPOSED_THRESHOLD: u8 = 250;
/// 亮度欠曝阈值：≤ 此值视为死黑
pub const UNDEREXPOSED_THRESHOLD: u8 = 5;
