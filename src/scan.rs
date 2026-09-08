//! 目录扫描 + EXIF 索引（M0）
//!
//! 扫描目录下所有 JPG/JPEG/ARW 文件，提取 EXIF 信息，
//! 并识别 JPG/ARW 同名配对关系。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

/// 单张照片的索引条目（CSV 行）
#[derive(Debug, Clone, Default, Serialize)]
pub struct PhotoEntry {
    pub path: String,
    pub filename: String,
    pub extension: String,
    pub is_raw: bool,
    /// 配对状态：JPG 与 ARW 同名时双方为 true
    pub has_pair: bool,
    pub date_time_original: String,
    pub camera_make: String,
    pub camera_model: String,
    pub lens_model: String,
    pub iso: String,
    pub f_number: String,
    pub shutter_speed: String,
    pub focal_length: String,
    // ---- 评分字段（score 子命令填充；scan 子命令为空）----
    pub sharpness_score: String,
    pub exposure_score: String,
    pub noise_score: String,
    pub composition_score: String,
    pub aesthetic_score: String,
    pub total_score: String,
    /// 星级（1-5；relative 模式为批次内相对排名，absolute 模式为总分阈值）
    pub stars: String,
    /// 检测到的人脸数
    pub faces: String,
    // ---- 连拍去重字段（score 子命令填充）----
    /// 连拍组号（0 = 非连拍）
    pub burst_group: String,
    /// 组内照片数
    pub burst_size: String,
    /// 子簇内排名（1 = 最优）
    pub burst_rank: String,
    /// 是否建议保留（true/false）
    pub burst_keep: String,
}

/// 是否为支持的照片文件扩展名
pub fn is_supported_file(name: &str) -> bool {
    matches!(extension_of(name).as_str(), "jpg" | "jpeg" | "arw")
}

/// 是否为 RAW 文件
pub fn is_raw_file(name: &str) -> bool {
    extension_of(name) == "arw"
}

/// 小写扩展名（不含点）
pub fn extension_of(name: &str) -> String {
    name.rsplit('.')
        .next()
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default()
}

/// 去掉扩展名的小写文件名（用于 JPG/ARW 配对）
pub fn stem_of(name: &str) -> String {
    match name.rfind('.') {
        Some(idx) => name[..idx].to_ascii_lowercase(),
        None => name.to_ascii_lowercase(),
    }
}

/// 去掉扩展名、**保留原始大小写**的文件名（用于侧车命名）
///
/// 侧车文件名必须与原文件同名（如 `DSC00001.xmp`），不能小写化：
/// 在大小写敏感的文件系统上 `dsc00001.xmp` 会被 Lightroom 视为不存在。
pub fn stem_raw_of(name: &str) -> String {
    match name.rfind('.') {
        Some(idx) => name[..idx].to_string(),
        None => name.to_string(),
    }
}

/// 配对/索引键：所在目录 + 文件主干（均小写）
///
/// **不能只用文件名主干**：索尼文件名编号在 DSC09999 后回绕到 DSC00001，
/// 上万张照片里不同目录必然出现同名文件；只按主干索引会让它们的
/// 分数、连拍分组、XMP 侧车互相覆盖（后扫描的覆盖先扫描的）。
pub fn pair_key(path: &Path, filename: &str) -> String {
    let dir = path
        .parent()
        .map(|p| p.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    format!("{dir}|{}", stem_of(filename))
}

impl PhotoEntry {
    /// 本条目用于配对/索引的键（目录 + 主干）
    pub fn pair_key(&self) -> String {
        pair_key(Path::new(&self.path), &self.filename)
    }
}

/// 递归扫描目录，返回排序后的照片索引
pub fn scan_directory(dir: &Path) -> Result<Vec<PhotoEntry>> {
    let mut files: Vec<PathBuf> = Vec::new();
    for entry in walkdir::WalkDir::new(dir).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_file() {
            let name = entry.file_name().to_string_lossy().to_string();
            if is_supported_file(&name) {
                files.push(entry.path().to_path_buf());
            }
        }
    }
    files.sort();

    // 构建 配对键 -> 扩展名集合，用于 JPG/ARW 配对判断
    // （键含目录：不同目录下的同名文件不得互相配对）
    let mut stem_exts: HashMap<String, HashSet<String>> = HashMap::new();
    for p in &files {
        let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
        stem_exts
            .entry(pair_key(p, &name))
            .or_default()
            .insert(extension_of(&name));
    }
    let has_pair = |key: &str, ext: &str| -> bool {
        let Some(exts) = stem_exts.get(key) else { return false };
        let is_jpg = exts.iter().any(|e| matches!(e.as_str(), "jpg" | "jpeg"));
        let is_arw = exts.contains("arw");
        match ext {
            "jpg" | "jpeg" => is_arw,
            "arw" => is_jpg,
            _ => false,
        }
    };

    let entries = files
        .iter()
        .map(|p| {
            let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
            let ext = extension_of(&name);
            let is_raw = is_raw_file(&name);
            let key = pair_key(p, &name);
            let paired = has_pair(&key, &ext);
            let exif = read_exif(p);
            PhotoEntry {
                path: p.display().to_string(),
                filename: name.clone(),
                extension: ext,
                is_raw,
                has_pair: paired,
                ..exif
            }
        })
        .collect();

    Ok(entries)
}

/// 读取 EXIF 关键字段；失败或缺失时返回默认（空）值，不阻断扫描
fn read_exif(path: &Path) -> PhotoEntry {
    let mut entry = PhotoEntry::default();
    let mut file = match std::fs::File::open(path) {
        Ok(f) => std::io::BufReader::new(f),
        Err(_) => return entry,
    };
    let buf = match exif::Reader::new().read_from_container(&mut file) {
        Ok(b) => b,
        Err(_) => return entry,
    };
    let field = |tag: exif::Tag, inp: exif::In| -> String {
        match buf.get_field(tag, inp) {
            Some(f) => match &f.value {
                // 多值字符串字段取第一个非空值（如 LensModel 尾部常带空串）
                exif::Value::Ascii(v) => v
                    .iter()
                    .find(|s| !s.is_empty())
                    .map(|s| String::from_utf8_lossy(s).trim().to_string())
                    .unwrap_or_default(),
                _ => f.display_value().to_string(),
            },
            None => String::new(),
        }
    };
    entry.date_time_original = field(exif::Tag::DateTimeOriginal, exif::In::PRIMARY);
    entry.camera_make = field(exif::Tag::Make, exif::In::PRIMARY);
    entry.camera_model = field(exif::Tag::Model, exif::In::PRIMARY);
    entry.lens_model = field(exif::Tag::LensModel, exif::In::PRIMARY);
    entry.iso = field(exif::Tag::PhotographicSensitivity, exif::In::PRIMARY);
    entry.f_number = field(exif::Tag::FNumber, exif::In::PRIMARY);
    entry.shutter_speed = field(exif::Tag::ExposureTime, exif::In::PRIMARY);
    entry.focal_length = field(exif::Tag::FocalLength, exif::In::PRIMARY);
    entry
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 配对/索引键必须含目录：索尼编号在 9999 处回绕，不同目录会有同名文件
    #[test]
    fn pair_key_separates_directories() {
        let a = pair_key(Path::new("roll1/DSC00001.JPG"), "DSC00001.JPG");
        let b = pair_key(Path::new("roll2/DSC00001.JPG"), "DSC00001.JPG");
        assert_ne!(a, b, "不同目录的同名文件不能共用键");
        // 同一目录下大小写/扩展名不同仍视为同一张（JPG 与 ARW 配对）
        assert_eq!(a, pair_key(Path::new("ROLL1/dsc00001.ARW"), "dsc00001.ARW"));
    }

    /// 侧车命名保留原始大小写
    #[test]
    fn stem_raw_keeps_case() {
        assert_eq!(stem_raw_of("DSC00001.ARW"), "DSC00001");
        assert_eq!(stem_of("DSC00001.ARW"), "dsc00001");
        assert_eq!(stem_raw_of("noext"), "noext");
    }
}
