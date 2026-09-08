//! 缩略图生成与磁盘缓存（M-UI1）
//!
//! 首次访问某张照片时从分析级解码（≤1024px，复用 decode.rs 的 box 降采样）
//! 缩到 320px 写入 `<照片根>/.firstcut/thumbs/`，此后直接命中文件。
//! 文件名含 path 哈希 + size + mtime，原文件变化自动换新缩略图。

use std::path::{Path, PathBuf};

use crate::cache;
use crate::decode;

/// 缩略图长边（px）
pub const THUMB_MAX_DIM: u32 = 320;

/// 缩略图缓存目录（`<root>/.firstcut/thumbs`）
pub fn thumb_dir(root: &Path) -> PathBuf {
    root.join(".firstcut").join("thumbs")
}

/// 稳定且不冲突的缓存文件名：path 哈希 + 文件指纹
fn cache_name(rel: &str, size: u64, mtime: i64) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    rel.hash(&mut h);
    format!("{:016x}-{}-{}.jpg", h.finish(), size, mtime)
}

/// 取缩略图字节：命中磁盘缓存直接读，否则解码生成（写缓存后返回）。
///
/// 返回 None = 原文件解码失败（坏图）；Err = IO 层问题。
pub fn get_or_create(root: &Path, rel: &str, full: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    let Some((size, mtime)) = cache::file_fingerprint(full) else {
        return Ok(None);
    };
    let dir = thumb_dir(root);
    let cache_file = dir.join(cache_name(rel, size, mtime));
    if let Ok(bytes) = std::fs::read(&cache_file) {
        return Ok(Some(bytes));
    }

    let Some(img) = decode::load_analysis_image(full)? else {
        return Ok(None);
    };
    let rgb = image::RgbImage::from_raw(img.width, img.height, img.rgb)
        .ok_or_else(|| anyhow::anyhow!("RGB 数据尺寸不一致"))?;
    // 保持长宽比缩到 320 盒内（卡片上的方形单元格由前端 object-fit 裁剪展示）
    let small =
        image::DynamicImage::ImageRgb8(rgb).resize(THUMB_MAX_DIM, THUMB_MAX_DIM, image::imageops::FilterType::Triangle);
    let rgb8 = small.to_rgb8();

    let mut buf = Vec::new();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 72);
    enc.encode(rgb8.as_raw(), rgb8.width(), rgb8.height(), image::ExtendedColorType::Rgb8)?;

    // 先写临时文件再改名，避免并发请求读到半截文件
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!(".tmp-{}", cache_name(rel, size, mtime)));
    std::fs::write(&tmp, &buf)?;
    let _ = std::fs::rename(&tmp, &cache_file);
    Ok(Some(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_name_stable_and_sensitive() {
        let a = cache_name("JPG/a.JPG", 100, 5);
        let b = cache_name("JPG/b.JPG", 100, 5);
        let a2 = cache_name("JPG/a.JPG", 100, 5);
        let a3 = cache_name("JPG/a.JPG", 101, 5);
        assert_eq!(a, a2, "同参数文件名稳定");
        assert_ne!(a, b, "不同路径不同名");
        assert_ne!(a, a3, "文件变化换新缓存");
    }
}
