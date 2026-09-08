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
/// JPEG 走 jpeg-decoder 全解码 + box 降采样快速路径，
/// 其他格式回退 image crate 全解码。
/// **EXIF Orientation 在降采样后应用**（1MP 图旋转成本远低于 33MP）。
pub fn load_analysis_image(path: &Path) -> Result<Option<AnalyzedImage>> {
    let orientation = read_orientation(path);

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
            if let Some(rgb) = decode_jpeg_to_rgb(&bytes) {
                return Ok(Some(build_analysis_image(apply_orientation(rgb, orientation))));
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
    Ok(Some(build_analysis_image(apply_orientation(small.to_rgb8(), orientation))))
}

/// 读取 EXIF Orientation（1-8）；缺失/失败返回 1（无变换）
pub fn read_orientation(path: &Path) -> u16 {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => std::io::BufReader::new(f),
        Err(_) => return 1,
    };
    match exif::Reader::new().read_from_container(&mut file) {
        Ok(exif) => exif
            .get_field(exif::Tag::Orientation, exif::In::PRIMARY)
            .and_then(|f| f.value.get_uint(0))
            .map(|v| v as u16)
            .filter(|v| (1..=8).contains(v))
            .unwrap_or(1),
        Err(_) => 1,
    }
}

/// 按 EXIF Orientation 旋转/镜像像素（1MP 图，成本可忽略）
///
/// 变换语义（对照 EXIF 规范与 image crate 的 rotate90=顺时针）：
/// - 5 = 转置（沿主对角线翻转）= `flip_horizontal(rotate90(img))`
/// - 7 = 反对角线翻转 = `flip_horizontal(rotate270(img))`
///
/// 注意：5/7 是**先旋转后镜像**；早期版本写成 `rotate90(flip_horizontal(img))`
/// 使两者互换（那是 rot180 与反对角线的差别），镜像+倒置的照片会方向错误。
pub fn apply_orientation(img: image::RgbImage, orientation: u16) -> image::RgbImage {
    use image::imageops::{flip_horizontal, flip_vertical, rotate180, rotate270, rotate90};
    match orientation {
        2 => flip_horizontal(&img),
        3 => rotate180(&img),
        4 => flip_vertical(&img),
        5 => flip_horizontal(&rotate90(&img)),
        6 => rotate90(&img),
        7 => flip_horizontal(&rotate270(&img)),
        8 => rotate270(&img),
        _ => img,
    }
}

/// JPEG 解码 + box 降采样，返回分析尺寸的 RGB（未应用 EXIF 旋转）
fn decode_jpeg_to_rgb(bytes: &[u8]) -> Option<image::RgbImage> {
    let mut decoder = jpeg_decoder::Decoder::new(bytes);
    let pixels = decoder.decode().ok()?;
    let info = decoder.info()?;
    let (w, h) = (info.width as u32, info.height as u32);
    if w == 0 || h == 0 {
        return None;
    }
    if w.max(h) <= ANALYSIS_MAX_DIM {
        return image::RgbImage::from_raw(w, h, pixels);
    }
    // box 降采样：步长 = ceil(长边/1024)
    let step = (w.max(h) as f32 / ANALYSIS_MAX_DIM as f32).ceil() as u32;
    let (nw, nh, out) = box_downsample(&pixels, w, h, step);
    let img = image::RgbImage::from_raw(nw, nh, out)?;
    // 极少情况下（非整数倍）仍略超 1024，用 image crate 收尾
    if nw.max(nh) > ANALYSIS_MAX_DIM {
        let scale = ANALYSIS_MAX_DIM as f32 / nw.max(nh) as f32;
        let rw = ((nw as f32 * scale).round() as u32).max(1);
        let rh = ((nh as f32 * scale).round() as u32).max(1);
        return Some(image::imageops::resize(&img, rw, rh, image::imageops::FilterType::Triangle));
    }
    Some(img)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 2×2 灰度图：1 2 / 3 4
    fn grid() -> image::RgbImage {
        image::RgbImage::from_raw(2, 2, vec![1, 1, 1, 2, 2, 2, 3, 3, 3, 4, 4, 4]).unwrap()
    }

    /// 行优先读回 R 通道
    fn values(img: &image::RgbImage) -> Vec<u8> {
        img.pixels().map(|p| p[0]).collect()
    }

    /// 8 种 EXIF Orientation 的像素变换（对照规范逐值锁定）
    ///
    /// 5 = 转置（主对角线）、7 = 反对角线翻转——这两者曾写反，
    /// 导致"镜像+旋转"的照片方向系统性错误。
    #[test]
    fn orientation_transforms_match_exif() {
        assert_eq!(values(&apply_orientation(grid(), 1)), vec![1, 2, 3, 4], "1 = 原样");
        assert_eq!(values(&apply_orientation(grid(), 2)), vec![2, 1, 4, 3], "2 = 水平镜像");
        assert_eq!(values(&apply_orientation(grid(), 3)), vec![4, 3, 2, 1], "3 = 180°");
        assert_eq!(values(&apply_orientation(grid(), 4)), vec![3, 4, 1, 2], "4 = 垂直镜像");
        assert_eq!(values(&apply_orientation(grid(), 5)), vec![1, 3, 2, 4], "5 = 转置");
        assert_eq!(values(&apply_orientation(grid(), 6)), vec![3, 1, 4, 2], "6 = 顺时针 90°");
        assert_eq!(values(&apply_orientation(grid(), 7)), vec![4, 2, 3, 1], "7 = 反对角线");
        assert_eq!(values(&apply_orientation(grid(), 8)), vec![2, 4, 1, 3], "8 = 逆时针 90°");
    }
}
