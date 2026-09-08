//! 目录扫描 + EXIF 索引（M0）
//!
//! 扫描目录下所有 JPG/JPEG/ARW 文件，提取 EXIF 信息，
//! 并识别 JPG/ARW 同名配对关系。

use std::collections::HashMap;
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
    /// 配对/索引键：同一张照片的 JPG/ARW 共享同一个值。
    /// 同目录配对为 `目录|主干`；无歧义的跨目录配对为 `cross|目录|主干`。
    /// 不写进 CSV（仅内部使用）。
    #[serde(skip_serializing)]
    pub pair_id: String,
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
    /// 评分数据是否可用（score 子命令填充：JPG 解码成功/成功映射到配对结果；
    /// 解码失败、无配对 ARW 等为 false；scan 子命令为空）
    pub analysis_ok: String,
    // ---- 连拍去重字段（score 子命令填充）----
    /// 连拍组号（0 = 非连拍）
    pub burst_group: String,
    /// 保留单元内张数
    pub burst_size: String,
    /// 保留单元内排名（1 = 最优）
    pub burst_rank: String,
    /// 是否建议保留（true/false）
    pub burst_keep: String,
    /// M9 姿态簇号（组内从 1 起；0 = 未启用自适应保留；非连拍为空）
    pub burst_pose: String,
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

/// 配对/索引键（单文件用）：所在目录 + 文件主干（均小写）
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
    /// 本条目用于配对/索引的键（同一张照片的 JPG/ARW 共享）
    pub fn pair_id(&self) -> &str {
        &self.pair_id
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

    // ---- 配对解析（两步：同目录优先，无歧义时跨目录兜底）----
    //
    // 背景：索尼编号在 DSC09999 后回绕，跨目录出现**不同**照片同名是常态；
    // 但同一张照片的 JPG/ARW 被分放到 JPG/ 与 RAW/ 两个子目录也是常态。
    // 所以不能简单"按目录隔离"，也不能"全树按主干合并"：
    //   1) 同目录内主干相同的 JPG/ARW 配对（主规则）；
    //   2) 剩余条目中，若某主干在**全树内恰好 1 个 JPG 和 1 个 ARW**且不在
    //      同一目录 → 跨目录配对；有歧义（≥2 个同名 JPG 或 ≥2 个同名 ARW）
    //      时**保持不配对**（宁可不配也不错误合并）。
    struct FileInfo {
        path: PathBuf,
        name: String,
        ext: String,
        is_raw: bool,
        dir: String,
        stem: String,
    }
    let infos: Vec<FileInfo> = files
        .iter()
        .map(|p| {
            let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
            FileInfo {
                path: p.clone(),
                ext: extension_of(&name),
                is_raw: is_raw_file(&name),
                dir: p
                    .parent()
                    .map(|d| d.to_string_lossy().to_ascii_lowercase())
                    .unwrap_or_default(),
                stem: stem_of(&name),
                name,
            }
        })
        .collect();

    let keyed: Vec<(String, String, bool)> =
        infos.iter().map(|f| (f.dir.clone(), f.stem.clone(), f.is_raw)).collect();
    let resolved = resolve_pairs(&keyed);
    let entries = infos
        .iter()
        .zip(resolved.iter())
        .map(|(f, (pair_id, paired))| {
            let exif = read_exif(&f.path);
            PhotoEntry {
                path: f.path.display().to_string(),
                filename: f.name.clone(),
                extension: f.ext.clone(),
                is_raw: f.is_raw,
                has_pair: *paired,
                pair_id: pair_id.clone(),
                ..exif
            }
        })
        .collect();

    Ok(entries)
}

/// 配对解析（纯函数，便于单元测试）
///
/// 输入每项为 `(目录, 主干, 是否 RAW)`，输出与输入等长的 `(pair_id, 是否配对)`。
/// 同一张照片的 JPG/ARW 共享同一个 `pair_id`。
fn resolve_pairs(items: &[(String, String, bool)]) -> Vec<(String, bool)> {
    // 同目录扩展名集合：(目录, 主干) -> (有 JPG, 有 ARW)
    let mut same_dir: HashMap<(String, String), (bool, bool)> = HashMap::new();
    // 全树按主干统计 JPG / RAW 下标
    let mut by_stem: HashMap<String, (Vec<usize>, Vec<usize>)> = HashMap::new();
    for (i, (dir, stem, is_raw)) in items.iter().enumerate() {
        let e = same_dir.entry((dir.clone(), stem.clone())).or_default();
        if *is_raw {
            e.1 = true;
            by_stem.entry(stem.clone()).or_default().1.push(i);
        } else {
            e.0 = true;
            by_stem.entry(stem.clone()).or_default().0.push(i);
        }
    }

    items
        .iter()
        .enumerate()
        .map(|(i, (dir, stem, is_raw))| {
            let local = same_dir.get(&(dir.clone(), stem.clone())).copied().unwrap_or_default();
            let same_dir_paired = if *is_raw { local.0 } else { local.1 };
            if same_dir_paired {
                return (format!("{dir}|{stem}"), true);
            }
            // 跨目录兜底：该主干在全树内唯一 JPG + 唯一 ARW，且分处不同目录
            let (jpgs, arws) = by_stem.get(stem).cloned().unwrap_or_default();
            if jpgs.len() == 1
                && arws.len() == 1
                && items[jpgs[0]].0 != items[arws[0]].0
            {
                (format!("cross|{}|{stem}", items[jpgs[0]].0), true)
            } else {
                // 未配对（或歧义）：仍给唯一的键，避免互相覆盖
                let _ = i;
                (format!("{dir}|{stem}"), false)
            }
        })
        .collect()
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

    fn item(dir: &str, stem: &str, is_raw: bool) -> (String, String, bool) {
        (dir.to_string(), stem.to_string(), is_raw)
    }

    /// 同目录配对（最常见布局）：JPG 与 ARW 同目录
    #[test]
    fn resolve_pairs_same_directory() {
        let items =
            vec![item("d", "dsc1", false), item("d", "dsc1", true), item("d", "dsc2", false)];
        let r = resolve_pairs(&items);
        assert_eq!(r[0].0, r[1].0, "同目录 JPG/ARW 应共享 pair_id");
        assert!(r[0].1 && r[1].1, "同目录 JPG/ARW 应标记为已配对");
        assert!(!r[2].1, "无对端的 JPG 不应配对");
        assert_ne!(r[2].0, r[0].0, "无对端条目键必须唯一");
    }

    /// 分目录布局（testpic 形态）：JPG/ 与 RAW/ 两个子目录
    #[test]
    fn resolve_pairs_across_directories() {
        let items = vec![item("jpg", "dsc1", false), item("raw", "dsc1", true)];
        let r = resolve_pairs(&items);
        assert_eq!(r[0].0, r[1].0, "无歧义跨目录应配对");
        assert!(r[0].1 && r[1].1);
    }

    /// 编号回绕：两个目录各有自己的同名 JPG+ARW → 各按同目录配对，不跨目录合并
    #[test]
    fn resolve_pairs_rollover_keeps_local_pairs() {
        let items = vec![
            item("roll1", "dsc1", false),
            item("roll1", "dsc1", true),
            item("roll2", "dsc1", false),
            item("roll2", "dsc1", true),
        ];
        let r = resolve_pairs(&items);
        assert_eq!(r[0].0, r[1].0, "roll1 内部配对");
        assert_eq!(r[2].0, r[3].0, "roll2 内部配对");
        assert_ne!(r[0].0, r[2].0, "两个回绕目录不得合并");
        assert!(r.iter().all(|(_, p)| *p));
    }

    /// 歧义场景：1 个 JPG + 2 个跨目录同名 ARW → 宁可不配
    #[test]
    fn resolve_pairs_ambiguous_stays_unpaired() {
        let items =
            vec![item("a", "dsc1", false), item("b", "dsc1", true), item("c", "dsc1", true)];
        let r = resolve_pairs(&items);
        assert!(r.iter().all(|(_, p)| !*p), "歧义时必须全部不配对");
        let ids: std::collections::HashSet<&String> = r.iter().map(|(k, _)| k).collect();
        assert_eq!(ids.len(), 3, "不配对条目键必须各自唯一");
    }

    /// 侧车命名保留原始大小写
    #[test]
    fn stem_raw_keeps_case() {
        assert_eq!(stem_raw_of("DSC00001.ARW"), "DSC00001");
        assert_eq!(stem_of("DSC00001.ARW"), "dsc00001");
        assert_eq!(stem_raw_of("noext"), "noext");
    }
}
