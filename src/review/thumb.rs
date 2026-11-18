//! 缩略图按需生成。每个磁盘槽只留一张，照片改名或更新不会无限增长。

use std::fs::File;
use std::path::Path;

use crate::decode;
use crate::storage::Folder;
use sha2::{Digest, Sha256};

pub const THUMB_MAX_DIM: u32 = 320;
const SLOT_COUNT: u16 = 8192;
const ENTRY_BYTES: u64 = 64 * 1024;

fn cache_key(photo: &str, size: u64, mtime: i64) -> (String, String) {
    let digest = Sha256::digest(photo.as_bytes());
    let slot = u16::from_le_bytes([digest[0], digest[1]]) % SLOT_COUNT;
    (
        format!("{slot:04x}.bin"),
        format!("{digest:x}:{size}:{mtime}\n"),
    )
}

pub fn get_or_create(root: &Path, rel: &str, photo: File) -> anyhow::Result<Option<Vec<u8>>> {
    let meta = photo.metadata()?;
    if meta.len() > decode::MAX_PHOTO_BYTES {
        return Ok(None);
    }
    let mtime = meta
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos() as i64;
    let (key, identity) = cache_key(rel, meta.len(), mtime);
    // Generated files are optional. If this album contains an unsafe or
    // read-only cache directory, don't touch it and render from the photo.
    let cache = Folder::open(root)
        .and_then(|dir| dir.state(true))
        .and_then(|dir| dir.child("thumbs", true))
        .and_then(|dir| dir.child("v2", true))
        .and_then(|dir| dir.file(&key))
        .ok();
    if let Some(cache) = &cache {
        if let Ok(Some(bytes)) = cache.read(ENTRY_BYTES) {
            if let Some(jpeg) = bytes.strip_prefix(identity.as_bytes()) {
                if jpeg.starts_with(&[0xff, 0xd8, 0xff]) {
                    return Ok(Some(jpeg.to_vec()));
                }
            }
        }
    }

    let Some(img) = decode::load_analysis_file(photo)? else {
        return Ok(None);
    };
    let rgb = image::RgbImage::from_raw(img.width, img.height, img.rgb)
        .ok_or_else(|| anyhow::anyhow!("RGB 数据尺寸不一致"))?;
    let small = image::DynamicImage::ImageRgb8(rgb)
        .resize(
            THUMB_MAX_DIM,
            THUMB_MAX_DIM,
            image::imageops::FilterType::Triangle,
        )
        .into_rgb8();
    let mut data = Vec::new();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut data, 72);
    enc.encode(
        small.as_raw(),
        small.width(),
        small.height(),
        image::ExtendedColorType::Rgb8,
    )?;

    if let Some(cache) = cache {
        if (identity.len() + data.len()) as u64 <= ENTRY_BYTES {
            let _ = cache.replace(|file| {
                use std::io::Write;
                file.write_all(identity.as_bytes())?;
                file.write_all(&data)
            });
        }
    }
    Ok(Some(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_photo_revision_reuses_the_same_slot_but_has_a_new_identity() {
        let (key, identity) = cache_key("JPG/a.JPG", 100, 5);
        let (updated_key, updated_identity) = cache_key("JPG/a.JPG", 101, 6);
        assert_eq!(key, updated_key);
        assert_ne!(identity, updated_identity);
    }
}
