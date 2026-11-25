//! 目录扫描 + EXIF 索引（M0）
//!
//! 扫描目录下所有 JPG/JPEG/ARW 文件，提取 EXIF 信息，
//! 并识别 JPG/ARW 同名配对关系。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// 单张照片的索引条目（CSV 行）
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
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
    #[serde(skip)]
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
    /// 建议曝光修正（EV，带符号；空 = 曝光在容差带内不给建议）。
    /// 同步写入 XMP（`firstcut:suggestedEV` + `crs:Exposure2012`），供 Lightroom / ACR 确认应用
    pub suggested_ev: String,
    /// 星级（1-5；relative 模式为批次内相对排名，absolute 模式为总分阈值）
    pub stars: String,
    /// algorithm 或 manual；空串表示尚未评分。
    pub rating_source: String,
    /// 检测到的人脸数
    pub faces: String,
    /// 评分数据是否可用（score 子命令填充：JPG 解码成功/成功映射到配对结果；
    /// 解码失败、无配对 ARW 等为 false；scan 子命令为空）
    pub analysis_ok: String,
    /// ai 或 pixel；未评分为空。
    pub analysis_mode: String,
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

/// 支持的主流相机 RAW 扩展名（全部小写；匹配时大小写不敏感）。
pub const RAW_EXTENSIONS: &[&str] = &[
    "arw", // Sony
    "nef", "nrw", // Nikon
    "cr2", "cr3", "crw", // Canon
    "rw2", // Panasonic / Lumix
    "raf", // Fujifilm
    "dng", // Leica / Adobe / Ricoh / Hasselblad
    "orf", // Olympus / OM System
    "pef", // Pentax
];

/// 是否为支持的 RAW 扩展名（不含点，大小写不敏感）
pub fn is_raw_extension(ext: &str) -> bool {
    RAW_EXTENSIONS.iter().any(|&r| r.eq_ignore_ascii_case(ext))
}

/// 是否为 JPEG 文件
pub fn is_jpeg_file(name: &str) -> bool {
    matches!(extension_of(name).as_str(), "jpg" | "jpeg")
}

/// 是否为支持的 RAW 文件
pub fn is_raw_file(name: &str) -> bool {
    is_raw_extension(&extension_of(name))
}

/// 是否为支持的照片文件扩展名（JPG/JPEG 或任意受支持的 RAW）
pub fn is_supported_file(name: &str) -> bool {
    is_jpeg_file(name) || is_raw_file(name)
}

/// 小写扩展名（不含点）
pub fn extension_of(name: &str) -> String {
    Path::new(name).extension().and_then(|suffix| suffix.to_str())
        .map(str::to_ascii_lowercase)
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

/// review 缩略图缓存目录名（整个子树跳过，防止 320px 缩略图被当照片评分）
const CACHE_DIR_NAME: &str = ".firstcut";

/// 导出目录标记与增量清单文件名（上级扫描遇到含该文件的子目录自动跳过）
pub const EXPORT_MARKER_NAME: &str = ".firstcut-export";

/// 递归扫描目录，返回排序后的照片索引
pub fn scan_directory(dir: &Path) -> Result<Vec<PhotoEntry>> {
    scan_directory_inner(dir, None, true, false, &|_, _, _| {})
}

/// 复核启动可沿用最近报告中的 EXIF；照片在报告之后被修改时仍重新读取。
pub fn scan_directory_with_exif_cache(dir: &Path, report: &Path) -> Result<Vec<PhotoEntry>> {
    scan_directory_inner(dir, load_report_exif(dir, report), true, false, &|_, _, _| {})
}

/// 复核界面专用：可关闭 RAW 关联，并报告真实扫描阶段和文件进度。
pub fn scan_review_directory(
    dir: &Path, report: &Path, include_raw: bool, skip_processed: bool,
    progress: &dyn Fn(&str, usize, usize),
) -> Result<Vec<PhotoEntry>> {
    progress("读取已有报告", 0, 0);
    scan_directory_inner(dir, load_report_exif(dir, report), include_raw, skip_processed, progress)
}

/// 评分跑批可复用相同的照片范围；CLI 默认仍扫描 JPG 与 RAW。
pub fn scan_directory_with_options(dir: &Path, include_raw: bool, skip_processed: bool) -> Result<Vec<PhotoEntry>> {
    scan_directory_inner(dir, None, include_raw, skip_processed, &|_, _, _| {})
}

/// 统一工作区扫描（支持单目录，以及双目录/双卡直连模式）。
pub fn scan_workspace(
    source: &crate::workspace::WorkspaceSource,
    state_dir: Option<&Path>,
    report: Option<&Path>,
    include_raw: bool,
    skip_processed: bool,
    progress: &dyn Fn(&str, usize, usize),
) -> Result<Vec<PhotoEntry>> {
    let exif_cache = match (state_dir, report) {
        (Some(st), Some(rep)) => {
            progress("读取已有报告", 0, 0);
            load_report_exif(st, rep)
        }
        _ => None,
    };
    match source {
        crate::workspace::WorkspaceSource::Single { root } => {
            scan_directory_inner(root, exif_cache, include_raw, skip_processed, progress)
        }
        crate::workspace::WorkspaceSource::Dual { jpg_root, raw_root } => {
            scan_dual_directories(jpg_root, raw_root, exif_cache, include_raw, skip_processed, progress)
        }
    }
}

fn load_report_exif(state_root: &Path, report: &Path) -> Option<(std::time::SystemTime, HashMap<String, PhotoEntry>)> {
    use std::io::Read;
    let file = crate::storage::FileRef::at(state_root, report, false).ok()?.open_read().ok()??;
    let meta = file.metadata().ok()?;
    const REPORT_LIMIT: u64 = 128 * 1024 * 1024;
    if meta.len() > REPORT_LIMIT { return None; }
    let report_time = meta.modified().ok()?;
    let mut reader = csv::Reader::from_reader(file.take(REPORT_LIMIT));
    let mut rows = HashMap::new();
    let unescape = crate::output::csv::unescape_spreadsheet_text;
    for row in reader.deserialize::<PhotoEntry>() {
        let mut row = row.ok()?;
        let raw_path = row.path.clone();
        row.path = unescape(&row.path).to_string();
        row.date_time_original = unescape(&row.date_time_original).to_string();
        row.camera_make = unescape(&row.camera_make).to_string();
        row.camera_model = unescape(&row.camera_model).to_string();
        row.lens_model = unescape(&row.lens_model).to_string();
        row.iso = unescape(&row.iso).to_string();
        row.f_number = unescape(&row.f_number).to_string();
        row.shutter_speed = unescape(&row.shutter_speed).to_string();
        row.focal_length = unescape(&row.focal_length).to_string();
        if raw_path != row.path {
            rows.insert(raw_path, row.clone());
        }
        rows.insert(row.path.clone(), row);
    }
    Some((report_time, rows))
}

fn collect_files_filtered(
    dir: &Path,
    skip_processed: bool,
    accept: &dyn Fn(&str) -> bool,
    progress: &dyn Fn(&str, usize, usize),
) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut walker = walkdir::WalkDir::new(dir).follow_links(false).into_iter();
    while let Some(entry) = walker.next() {
        let entry = entry?;
        if entry.depth() > 0 && entry.file_name().to_string_lossy().eq_ignore_ascii_case(CACHE_DIR_NAME) {
            // 不递归进缓存目录（walkdir 默认会继续展开子目录）
            if entry.file_type().is_dir() { walker.skip_current_dir(); }
            continue;
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if entry.depth() > 0 && entry.metadata()?.file_attributes()
                & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                if entry.file_type().is_dir() { walker.skip_current_dir(); }
                continue;
            }
        }
        if skip_processed && entry.depth() > 0 && entry.file_type().is_dir()
            && entry.file_name().to_string_lossy().eq_ignore_ascii_case("Processed") {
            walker.skip_current_dir();
            continue;
        }
        if entry.depth() > 0 && entry.file_type().is_dir()
            && std::fs::symlink_metadata(entry.path().join(EXPORT_MARKER_NAME))
                .is_ok_and(|meta| meta.is_file())
        {
            walker.skip_current_dir();
            continue;
        }
        if entry.file_type().is_file() {
            // Review identities must round-trip through UTF-8 and refer to
            // exactly one file. Don't alias a lossy display name.
            let Some(name) = entry.file_name().to_str() else { continue; };
            if entry.path().to_str().is_none() { continue; }
            if accept(name) {
                files.push(entry.path().to_path_buf());
                if files.len() % 100 == 0 { progress("查找照片文件", files.len(), 0); }
            }
        }
    }
    files.sort();
    Ok(files)
}

fn scan_directory_inner(
    dir: &Path,
    exif_cache: Option<(std::time::SystemTime, HashMap<String, PhotoEntry>)>,
    include_raw: bool,
    skip_processed: bool,
    progress: &dyn Fn(&str, usize, usize),
) -> Result<Vec<PhotoEntry>> {
    progress("查找照片文件", 0, 0);
    let files = collect_files_filtered(
        dir,
        skip_processed,
        &|name| is_supported_file(name) && (include_raw || !is_raw_file(name)),
        progress,
    )?;
    progress("读取照片元数据", 0, files.len());

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
        .enumerate()
        .map(|(index, (f, (pair_id, paired)))| {
            let exif = exif_cache.as_ref().and_then(|(report_time, rows)| {
                let modified = f.path.metadata().ok()?.modified().ok()?;
                if modified > *report_time { return None; }
                let old = rows.get(&f.path.display().to_string())?;
                Some(PhotoEntry {
                    date_time_original: old.date_time_original.clone(),
                    camera_make: old.camera_make.clone(),
                    camera_model: old.camera_model.clone(),
                    lens_model: old.lens_model.clone(),
                    iso: old.iso.clone(),
                    f_number: old.f_number.clone(),
                    shutter_speed: old.shutter_speed.clone(),
                    focal_length: old.focal_length.clone(),
                    ..Default::default()
                })
            }).unwrap_or_else(|| read_exif(dir, &f.path));
            if (index + 1) % 25 == 0 || index + 1 == infos.len() {
                progress("读取照片元数据", index + 1, infos.len());
            }
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

fn scan_dual_directories(
    jpg_root: &Path,
    raw_root: &Path,
    exif_cache: Option<(std::time::SystemTime, HashMap<String, PhotoEntry>)>,
    include_raw: bool,
    skip_processed: bool,
    progress: &dyn Fn(&str, usize, usize),
) -> Result<Vec<PhotoEntry>> {
    progress("查找照片文件", 0, 0);
    let jpg_files = collect_files_filtered(jpg_root, skip_processed, &is_jpeg_file, progress)?;
    let raw_files = if include_raw {
        collect_files_filtered(raw_root, skip_processed, &is_raw_file, progress)?
    } else {
        Vec::new()
    };
    let total_files = jpg_files.len() + raw_files.len();
    progress("读取照片元数据", 0, total_files);

    let rel_dir_lower = |root: &Path, file: &Path| -> String {
        file.strip_prefix(root)
            .ok()
            .and_then(|rel| rel.parent())
            .map(|p| p.to_string_lossy().replace('\\', "/").trim_matches('/').to_ascii_lowercase())
            .unwrap_or_default()
    };

    let read_entry = |root: &Path, path: &Path, is_raw: bool, done_idx: usize| -> PhotoEntry {
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        let exif = exif_cache
            .as_ref()
            .and_then(|(report_time, rows)| {
                let modified = path.metadata().ok()?.modified().ok()?;
                if modified > *report_time {
                    return None;
                }
                let old = rows.get(&path.display().to_string())?;
                Some(PhotoEntry {
                    date_time_original: old.date_time_original.clone(),
                    camera_make: old.camera_make.clone(),
                    camera_model: old.camera_model.clone(),
                    lens_model: old.lens_model.clone(),
                    iso: old.iso.clone(),
                    f_number: old.f_number.clone(),
                    shutter_speed: old.shutter_speed.clone(),
                    focal_length: old.focal_length.clone(),
                    ..Default::default()
                })
            })
            .unwrap_or_else(|| read_exif(root, path));
        if (done_idx + 1) % 25 == 0 || done_idx + 1 == total_files {
            progress("读取照片元数据", done_idx + 1, total_files);
        }
        PhotoEntry {
            path: path.display().to_string(),
            extension: extension_of(&name),
            filename: name,
            is_raw,
            ..exif
        }
    };

    let mut jpg_entries: Vec<PhotoEntry> = jpg_files
        .iter()
        .enumerate()
        .map(|(i, p)| read_entry(jpg_root, p, false, i))
        .collect();
    let mut raw_entries: Vec<PhotoEntry> = raw_files
        .iter()
        .enumerate()
        .map(|(i, p)| read_entry(raw_root, p, true, jpg_files.len() + i))
        .collect();

    let jpg_keys: Vec<(String, String, String)> = jpg_files
        .iter()
        .zip(jpg_entries.iter())
        .map(|(p, e)| {
            (
                rel_dir_lower(jpg_root, p),
                stem_of(&e.filename),
                e.date_time_original.trim().to_string(),
            )
        })
        .collect();
    let raw_keys: Vec<(String, String, String)> = raw_files
        .iter()
        .zip(raw_entries.iter())
        .map(|(p, e)| {
            (
                rel_dir_lower(raw_root, p),
                stem_of(&e.filename),
                e.date_time_original.trim().to_string(),
            )
        })
        .collect();

    let (jpg_resolved, raw_resolved) = resolve_dual_pairs(&jpg_keys, &raw_keys);
    for (entry, (pair_id, paired)) in jpg_entries.iter_mut().zip(jpg_resolved.into_iter()) {
        entry.pair_id = pair_id;
        entry.has_pair = paired;
    }
    for (entry, (pair_id, paired)) in raw_entries.iter_mut().zip(raw_resolved.into_iter()) {
        entry.pair_id = pair_id;
        entry.has_pair = paired;
    }

    let mut combined = Vec::with_capacity(jpg_entries.len() + raw_entries.len());
    combined.extend(jpg_entries);
    combined.extend(raw_entries);
    combined.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(combined)
}

/// 双目录三级配对解析（纯函数，便于单元测试）：
/// 输入 `(rel_dir_lower, stem_lower, datetime_trimmed)`：
/// 1. 同相对子目录 + 同主干唯一配对（防 `9999 -> 0001` 回绕）；
/// 2. 剩余条目中全树唯一主干跨目录兜底（兼容两张卡 `100MSDCF` / `104MSDCF` 目录号不同）；
/// 3. 仍有同名歧义时，用 `stem + 非空 EXIF DateTimeOriginal` 精确唯一配对；其余保持不配对。
fn resolve_dual_pairs(
    jpgs: &[(String, String, String)],
    raws: &[(String, String, String)],
) -> (Vec<(String, bool)>, Vec<(String, bool)>) {
    let mut jpg_out: Vec<Option<(String, bool)>> = vec![None; jpgs.len()];
    let mut raw_out: Vec<Option<(String, bool)>> = vec![None; raws.len()];

    // Level 1: (rel_dir, stem)
    let mut jpg_by_dir_stem: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
    let mut raw_by_dir_stem: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
    for (i, (dir, stem, _)) in jpgs.iter().enumerate() {
        jpg_by_dir_stem.entry((dir.as_str(), stem.as_str())).or_default().push(i);
    }
    for (i, (dir, stem, _)) in raws.iter().enumerate() {
        raw_by_dir_stem.entry((dir.as_str(), stem.as_str())).or_default().push(i);
    }
    for ((dir, stem), j_idxs) in &jpg_by_dir_stem {
        if j_idxs.len() == 1 {
            if let Some(r_idxs) = raw_by_dir_stem.get(&(*dir, *stem)) {
                if r_idxs.len() == 1 {
                    let pair_id = format!("dual|{dir}|{stem}");
                    jpg_out[j_idxs[0]] = Some((pair_id.clone(), true));
                    raw_out[r_idxs[0]] = Some((pair_id, true));
                }
            }
        }
    }

    // Level 2: unique remaining stem across both trees
    let mut jpg_by_stem: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut raw_by_stem: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, (_, stem, _)) in jpgs.iter().enumerate() {
        if jpg_out[i].is_none() {
            jpg_by_stem.entry(stem.as_str()).or_default().push(i);
        }
    }
    for (i, (_, stem, _)) in raws.iter().enumerate() {
        if raw_out[i].is_none() {
            raw_by_stem.entry(stem.as_str()).or_default().push(i);
        }
    }
    for (stem, j_idxs) in &jpg_by_stem {
        if j_idxs.len() == 1 {
            if let Some(r_idxs) = raw_by_stem.get(stem) {
                if r_idxs.len() == 1 {
                    let ji = j_idxs[0];
                    let ri = r_idxs[0];
                    let pair_id = format!("dual|{}|{stem}", jpgs[ji].0);
                    jpg_out[ji] = Some((pair_id.clone(), true));
                    raw_out[ri] = Some((pair_id, true));
                }
            }
        }
    }

    // Level 3: unique (stem, non-empty DateTimeOriginal) among remaining
    let mut jpg_by_dt: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
    let mut raw_by_dt: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
    for (i, (_, stem, dt)) in jpgs.iter().enumerate() {
        if jpg_out[i].is_none() && !dt.is_empty() {
            jpg_by_dt.entry((stem.as_str(), dt.as_str())).or_default().push(i);
        }
    }
    for (i, (_, stem, dt)) in raws.iter().enumerate() {
        if raw_out[i].is_none() && !dt.is_empty() {
            raw_by_dt.entry((stem.as_str(), dt.as_str())).or_default().push(i);
        }
    }
    for ((stem, _), j_idxs) in &jpg_by_dt {
        if j_idxs.len() == 1 {
            if let Some(r_idxs) = raw_by_dt.get(&(*stem, jpgs[j_idxs[0]].2.as_str())) {
                if r_idxs.len() == 1 {
                    let ji = j_idxs[0];
                    let ri = r_idxs[0];
                    let pair_id = format!("dual|{}|{stem}", jpgs[ji].0);
                    jpg_out[ji] = Some((pair_id.clone(), true));
                    raw_out[ri] = Some((pair_id, true));
                }
            }
        }
    }

    let jpg_final = jpg_out
        .into_iter()
        .enumerate()
        .map(|(i, res)| res.unwrap_or_else(|| (format!("jpg|{}|{}", jpgs[i].0, jpgs[i].1), false)))
        .collect();
    let raw_final = raw_out
        .into_iter()
        .enumerate()
        .map(|(i, res)| res.unwrap_or_else(|| (format!("raw|{}|{}", raws[i].0, raws[i].1), false)))
        .collect();
    (jpg_final, raw_final)
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
fn read_exif(root: &Path, path: &Path) -> PhotoEntry {
    let mut entry = PhotoEntry::default();
    let mut file = match crate::storage::FileRef::inside(root, path).and_then(|file| file.open_read()) {
        Ok(Some(f)) => std::io::BufReader::new(f),
        _ => return entry,
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

    #[test]
    fn review_reuses_recent_report_exif() {
        let dir = std::env::temp_dir().join(format!("firstcut_exif_cache_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let jpg = dir.join("a.jpg");
        std::fs::write(&jpg, b"not a real jpeg").unwrap();
        let report = dir.join("report.csv");
        let entry = PhotoEntry {
            path: jpg.display().to_string(), filename: "a.jpg".into(),
            extension: "jpg".into(), date_time_original: "2025:08:28 10:00:00".into(),
            lens_model: "+FE 50mm".into(),
            ..Default::default()
        };
        crate::output::csv::write_csv_in(&dir, &report, &[entry], true).unwrap();

        let entries = scan_directory_with_exif_cache(&dir, &report).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].date_time_original, "2025:08:28 10:00:00");
        assert_eq!(entries[0].lens_model, "+FE 50mm", "表格公式保护前缀应还原");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn review_can_disable_raw_pairing_and_reports_progress() {
        let dir = std::env::temp_dir().join(format!("firstcut_raw_option_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("JPG")).unwrap();
        std::fs::create_dir_all(dir.join("RAW")).unwrap();
        std::fs::create_dir_all(dir.join("Processed")).unwrap();
        std::fs::write(dir.join("JPG/a.jpg"), b"jpg").unwrap();
        std::fs::write(dir.join("RAW/a.ARW"), b"raw").unwrap();
        std::fs::write(dir.join("Processed/export.jpg"), b"jpg").unwrap();
        let stages = std::sync::Mutex::new(Vec::new());
        let progress = |phase: &str, done, total| {
            stages.lock().unwrap().push((phase.to_string(), done, total));
        };
        let jpg_only = scan_review_directory(&dir, &dir.join("none.csv"), false, true, &progress).unwrap();
        assert_eq!(jpg_only.len(), 1);
        assert!(!jpg_only[0].has_pair);
        assert!(stages.lock().unwrap().iter().any(|(phase, done, total)|
            phase == "读取照片元数据" && *done == 1 && *total == 1));
        let paired = scan_review_directory(&dir, &dir.join("none.csv"), true, true, &progress).unwrap();
        assert_eq!(paired.len(), 2);
        assert!(paired.iter().all(|entry| entry.has_pair));
        let with_outputs = scan_review_directory(&dir, &dir.join("none.csv"), true, false, &progress).unwrap();
        assert_eq!(with_outputs.len(), 3);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn export_marker_skips_nested_export_dir_but_allows_direct_open() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("album");
        let selected = root.join("Selected");
        std::fs::create_dir_all(root.join("JPG")).unwrap();
        std::fs::create_dir_all(root.join("RAW")).unwrap();
        std::fs::create_dir_all(selected.join("JPG")).unwrap();
        std::fs::create_dir_all(selected.join("RAW")).unwrap();
        std::fs::write(root.join("JPG/DSC00001.JPG"), b"jpg").unwrap();
        std::fs::write(root.join("RAW/DSC00001.ARW"), b"raw").unwrap();
        std::fs::write(selected.join(EXPORT_MARKER_NAME), b"{}").unwrap();
        std::fs::write(selected.join("JPG/DSC00001.JPG"), b"jpg").unwrap();
        std::fs::write(selected.join("RAW/DSC00001.ARW"), b"raw").unwrap();

        // 扫描上级相册时跳过 Selected/，不会翻倍也不破坏原有配对
        let parent_entries = scan_directory(&root).unwrap();
        assert_eq!(parent_entries.len(), 2);
        assert!(parent_entries.iter().all(|e| e.has_pair));

        // 直接把 Selected/ 当作根目录打开时仍可正常浏览
        let direct_entries = scan_directory(&selected).unwrap();
        assert_eq!(direct_entries.len(), 2);
        assert!(direct_entries.iter().all(|e| e.has_pair));
    }

    #[test]
    fn multi_brand_raw_extensions_are_recognized_case_insensitively() {
        for ext in [
            "ARW", "arw", "NEF", "nef", "NRW", "nrw", "CR2", "CR3", "crw", "RW2", "RAF", "DNG",
            "ORF", "PEF",
        ] {
            let name = format!("IMG_0001.{ext}");
            assert!(is_raw_file(&name), "应识别 RAW 扩展名: {name}");
            assert!(is_supported_file(&name));
            assert!(!is_jpeg_file(&name));
        }
        assert!(is_jpeg_file("IMG_0001.JPG"));
        assert!(is_jpeg_file("img.jpeg"));
        assert!(!is_raw_file("IMG_0001.JPG"));
        assert!(!is_supported_file("notes.xmp"));
    }

    #[test]
    fn dual_pairs_three_levels_handle_rollover_cross_folder_and_exif_disambiguation() {
        let jpgs = vec![
            // Level 1: 同子目录同主干（即便回绕重名也能各自正确配对）
            ("100msdcf".into(), "dsc0001".into(), "2026:11:25 10:00:00".into()),
            ("101msdcf".into(), "dsc0001".into(), "2026:11:25 12:00:00".into()),
            // Level 2: 两张卡子目录不同，但主干全局唯一
            ("100msdcf".into(), "dsc0002".into(), "".into()),
            // Level 3: 子目录不同且回绕重名，依靠 EXIF DateTimeOriginal 唯一消歧
            ("102msdcf".into(), "dsc0003".into(), "2026:11:25 13:00:00".into()),
            ("103msdcf".into(), "dsc0003".into(), "2026:11:25 14:00:00".into()),
            // 仍无法消歧（无 EXIF）：保持不配对
            ("104msdcf".into(), "dsc0004".into(), "".into()),
        ];
        let raws = vec![
            ("100msdcf".into(), "dsc0001".into(), "2026:11:25 10:00:00".into()),
            ("101msdcf".into(), "dsc0001".into(), "2026:11:25 12:00:00".into()),
            ("200msdcf".into(), "dsc0002".into(), "".into()),
            ("300msdcf".into(), "dsc0003".into(), "2026:11:25 14:00:00".into()),
            ("301msdcf".into(), "dsc0003".into(), "2026:11:25 13:00:00".into()),
            ("400msdcf".into(), "dsc0004".into(), "".into()),
            ("401msdcf".into(), "dsc0004".into(), "".into()),
        ];
        let (jr, rr) = resolve_dual_pairs(&jpgs, &raws);
        // Level 1
        assert_eq!(jr[0].0, rr[0].0);
        assert_eq!(jr[1].0, rr[1].0);
        assert_ne!(jr[0].0, jr[1].0);
        // Level 2
        assert!(jr[2].1 && rr[2].1);
        assert_eq!(jr[2].0, rr[2].0);
        // Level 3（注意 raws[3] 时间 14:00 对应 jpgs[4]，raws[4] 时间 13:00 对应 jpgs[3]）
        assert!(jr[3].1 && rr[4].1);
        assert_eq!(jr[3].0, rr[4].0);
        assert!(jr[4].1 && rr[3].1);
        assert_eq!(jr[4].0, rr[3].0);
        assert_ne!(jr[3].0, jr[4].0);
        // Ambiguous without EXIF stays unpaired
        assert!(!jr[5].1 && !rr[5].1 && !rr[6].1);
    }

    #[test]
    fn scan_workspace_dual_isolates_formats_per_card() {
        let temp = tempfile::tempdir().unwrap();
        let jpg_card = temp.path().join("jpg_card");
        let raw_card = temp.path().join("raw_card");
        std::fs::create_dir_all(jpg_card.join("100MSDCF")).unwrap();
        std::fs::create_dir_all(raw_card.join("104MSDCF")).unwrap();
        std::fs::write(jpg_card.join("100MSDCF/IMG_0001.JPG"), b"jpg").unwrap();
        // 故意在 JPG 卡放一个 RAW、RAW 卡放一个 JPG，验证双目录严格按角色过滤
        std::fs::write(jpg_card.join("100MSDCF/WRONG.NEF"), b"wrong-raw").unwrap();
        std::fs::write(raw_card.join("104MSDCF/IMG_0001.CR3"), b"raw").unwrap();
        std::fs::write(raw_card.join("104MSDCF/WRONG.JPG"), b"wrong-jpg").unwrap();

        let source = crate::workspace::WorkspaceSource::dual(&jpg_card, &raw_card).unwrap();
        let entries = scan_workspace(&source, None, None, true, true, &|_, _, _| {}).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|e| e.has_pair));
        assert!(entries.iter().any(|e| !e.is_raw && e.filename == "IMG_0001.JPG"));
        assert!(entries.iter().any(|e| e.is_raw && e.filename == "IMG_0001.CR3"));
    }
}
