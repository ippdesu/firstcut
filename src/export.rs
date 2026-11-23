//! 导出所选照片（JPG / RAW / XMP）到子目录或已授权本地目录。
//!
//! 设计原则：
//! - **严格非破坏**：绝不删除或修改原目录照片，也绝不删除目标目录里既有文件；
//!   已在导出目录中被 PS/ACR 修改过的 XMP 默认保留不覆盖。
//! - **防二次扫描反吃**：导出目录内写入 `.firstcut-export` 清单文件；上级相册扫描时
//!   自动跳过含该标记的子目录，且拒绝把导出目标设在原片目录或 `.firstcut/` 内。
//! - **安全与取消安全**：通过 `storage::Folder` / `FileRef` 逐级无链接访问、流式分块
//!   写入同目录临时文件并原子重命名；中途取消在当前文件完成后立即停止，不留半截文件。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cache::{self, ScoreCache};
use crate::config::ScoreConfig;
use crate::scan::{self, PhotoEntry, EXPORT_MARKER_NAME};
use crate::score::{self, AnalysisResult, PixelScores};
use crate::storage::{self, FileRef, Folder};

/// 预留磁盘安全余量（64 MiB），低于此余量在预览阶段发出空间警告。
const DISK_HEADROOM_BYTES: u64 = 64 * 1024 * 1024;
const MAX_XMP_BYTES: u64 = 16 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 32 * 1024 * 1024;

/// 导出目录布局
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportLayout {
    /// 默认：按类型分 `JPG/` 与 `RAW/` 子目录，两端各自附带 `.xmp`
    SplitByType,
    /// 扁平化：全部文件放入同一导出目录（JPG/RAW 共用一份 `.xmp`）
    Flat,
    /// 保持原相对目录结构
    PreserveTree,
}

impl Default for ExportLayout {
    fn default() -> Self {
        Self::SplitByType
    }
}

/// 导出目标位置
#[derive(Debug, Clone)]
pub enum ExportTarget {
    /// 相册根目录内的相对子目录（默认 `Selected`）
    AlbumSubdir(PathBuf),
    /// 显式本地目录（CLI `--to` 或桌面端原生文件夹弹窗授权）
    ExternalDir(PathBuf),
}

/// 导出选项
#[derive(Debug, Clone)]
pub struct ExportOptions {
    pub target: ExportTarget,
    pub layout: ExportLayout,
    pub include_jpg: bool,
    pub include_raw: bool,
    pub include_xmp: bool,
    /// 仅对尚无源侧车、在导出目录中新生成的 XMP 生效（默认 false，遵守方案 A）
    pub xmp_exposure: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            target: ExportTarget::AlbumSubdir(PathBuf::from("Selected")),
            layout: ExportLayout::SplitByType,
            include_jpg: true,
            include_raw: true,
            include_xmp: true,
            xmp_exposure: false,
        }
    }
}

/// CLI / 通用连拍组筛选
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GroupFilter {
    #[default]
    All,
    BurstOnly,
    SingleOnly,
    Group(usize),
}

/// CLI `export` 使用的筛选条件（与 Web UI 语义对齐）
#[derive(Debug, Clone, Default)]
pub struct ExportFilter {
    pub stars: Option<Vec<u8>>,
    pub min_stars: Option<u8>,
    /// 仅保留：非连拍单张全部保留，连拍组内仅保留有效 `keep = true` 的帧
    pub keep_only: bool,
    pub group_filter: GroupFilter,
    pub faces_only: bool,
}

/// 用于导出目录内新生成 XMP 的照片元数据
#[derive(Debug, Clone)]
pub struct ExportXmpMeta {
    pub entry: PhotoEntry,
    pub scores: PixelScores,
    pub total: f64,
    pub rating: u8,
    pub suggested_ev: Option<f64>,
}

/// 单张待导出照片（以 JPG/RAW 配对为粒度）
#[derive(Debug, Clone)]
pub struct ExportPhotoItem {
    pub pair_id: String,
    /// 相对照片根目录的 JPG 路径（正斜杠）
    pub jpg_rel: Option<String>,
    /// 相对照片根目录的配对 RAW 路径（正斜杠）
    pub raw_rel: Option<String>,
    pub xmp_meta: Option<ExportXmpMeta>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportFileKind {
    Jpg,
    Raw,
    Xmp,
}

impl ExportFileKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Jpg => "jpg",
            Self::Raw => "raw",
            Self::Xmp => "xmp",
        }
    }
}

/// `.firstcut-export` 清单文件结构（兼具防二次扫描标记与增量/人工修改检测功能）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExportManifest {
    pub version: u32,
    pub source_root: String,
    pub updated_at_unix_ms: u64,
    pub files: BTreeMap<String, ManifestEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub size: u64,
    pub mtime_nanos: i64,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// 记录导出该目标文件的源照片相对路径，防止不同批次把另一张同名照片误覆盖已有副本。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_rel: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CollisionNote {
    pub source: String,
    pub exported_stem: String,
}

/// 导出前预览摘要（返回给 Web UI 弹窗与 CLI `--dry-run`）
#[derive(Debug, Clone, Serialize)]
pub struct ExportPreview {
    pub target_display: String,
    pub in_album: bool,
    pub selected_photos: usize,
    pub jpg_to_copy: usize,
    pub raw_to_copy: usize,
    pub xmp_to_write: usize,
    pub xmp_generated: usize,
    pub skipped_unchanged: usize,
    pub protected_existing: usize,
    pub total_copy_bytes: u64,
    pub available_bytes: Option<u64>,
    pub low_disk_space: bool,
    pub missing_raw_count: usize,
    pub missing_raw_samples: Vec<String>,
    pub missing_xmp_unscored: usize,
    pub renamed_count: usize,
    pub renamed_samples: Vec<CollisionNote>,
    pub protected_samples: Vec<String>,
    pub warnings: Vec<String>,
}

/// 导出执行过程中的进度与日志事件
pub enum ExportEvent {
    Info(String),
    Progress {
        done: usize,
        total: usize,
        bytes_done: u64,
        bytes_total: u64,
        current_file: String,
    },
}

/// 导出完成摘要
#[derive(Debug, Clone, Serialize)]
pub struct ExportSummary {
    pub target_display: String,
    pub target_path: PathBuf,
    pub in_album: bool,
    pub jpg_copied: usize,
    pub raw_copied: usize,
    pub xmp_written: usize,
    pub skipped_unchanged: usize,
    pub protected_existing: usize,
    pub failed: usize,
    pub bytes_written: u64,
    pub cancelled: bool,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone)]
enum ResolvedTarget {
    AlbumSubdir { rel: PathBuf, display: PathBuf },
    ExternalDir { path: PathBuf },
}

impl ResolvedTarget {
    fn display_path(&self) -> &Path {
        match self {
            Self::AlbumSubdir { display, .. } => display,
            Self::ExternalDir { path } => path,
        }
    }

    fn is_in_album(&self) -> bool {
        matches!(self, Self::AlbumSubdir { .. })
    }

    fn open_folder(&self, root: &Path, create: bool) -> std::io::Result<Folder> {
        match self {
            Self::AlbumSubdir { rel, .. } => Folder::open_album_subdir(root, rel, create),
            Self::ExternalDir { path } => Folder::open_or_create_explicit(path, create),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileAction {
    Write,
    SkipUnchanged,
    ProtectExisting,
}

#[derive(Debug, Clone)]
enum PlannedSource {
    CopyPhoto {
        src_rel: String,
        src_size: u64,
        src_mtime: Option<SystemTime>,
    },
    WriteXmp {
        bytes: Vec<u8>,
        sha256: String,
        generated: bool,
    },
}

#[derive(Debug, Clone)]
struct PlannedFile {
    dst_rel: String,
    source_identity: String,
    kind: ExportFileKind,
    source: PlannedSource,
    action: FileAction,
}

/// 完整的导出执行计划（由 `plan_export` 产出，可供预览并直接交给 `run_export` 执行）
pub struct ExportPlan {
    root: PathBuf,
    target: ResolvedTarget,
    files: Vec<PlannedFile>,
    pub preview: ExportPreview,
}

impl ExportPlan {
    pub fn target_path(&self) -> &Path {
        self.target.display_path()
    }
}

/// 供 CLI `export` 使用：扫描目录、读缓存与统一决定库、按筛选条件构建导出列表。
pub fn collect_items_for_cli(
    root: &Path,
    cfg: &ScoreConfig,
    cache_path: &Path,
    filter: &ExportFilter,
) -> Result<(Vec<ExportPhotoItem>, Vec<String>)> {
    if filter.stars.is_some() && filter.min_stars.is_some() {
        bail!("--stars 与 --min-stars 不能同时指定");
    }
    if let Some(m) = filter.min_stars {
        anyhow::ensure!((1..=5).contains(&m), "--min-stars 必须在 1~5 之间");
    }
    if let Some(list) = &filter.stars {
        anyhow::ensure!(!list.is_empty(), "--stars 不能为空");
        for &s in list {
            anyhow::ensure!((1..=5).contains(&s), "--stars 中的星级必须在 1~5 之间");
        }
    }

    let entries = scan::scan_directory_with_options(root, true, true)?;
    let all_source_rels: Vec<String> = entries
        .iter()
        .map(|e| normalize_rel(root, Path::new(&e.path)))
        .collect();

    let pixel_hash = crate::config::analysis_fingerprint(cfg, false);
    let ai_hash = crate::config::analysis_fingerprint(cfg, true);
    let legacy_ai_hash = crate::ai::ensure_models()
        .ok()
        .map(|_| crate::config::legacy_ai_fingerprint(cfg));

    let mut analyzed: HashMap<String, AnalysisResult> = HashMap::new();
    if let Some(cache) = ScoreCache::open_in(root, cache_path, pixel_hash, false)? {
        for e in entries.iter().filter(|e| !e.is_raw) {
            let full = Path::new(&e.path);
            let Some((size, mtime)) = FileRef::inside(root, full)
                .ok()
                .and_then(|file| file.open_read().ok().flatten())
                .and_then(|file| file.metadata().ok())
                .and_then(|meta| cache::metadata_fingerprint(&meta))
            else {
                continue;
            };
            let matches_any = |row: &cache::CacheRow| -> bool {
                row.matches(size, mtime, cache::CACHE_VERSION, ai_hash)
                    || legacy_ai_hash
                        .is_some_and(|h| row.matches(size, mtime, cache::CACHE_VERSION, h))
                    || row.matches(size, mtime, cache::CACHE_VERSION, pixel_hash)
            };
            if let Some((row, _)) = cache::find_matching(cache.rows(), root, &e.path, matches_any) {
                analyzed.insert(e.pair_id().to_string(), row.result);
            }
        }
    }

    let selection = crate::selection::build(root, &entries, &analyzed, cfg, None)?;

    let mut raw_by_pair: HashMap<&str, &PhotoEntry> = HashMap::new();
    for e in entries.iter().filter(|e| e.is_raw) {
        raw_by_pair.entry(e.pair_id()).or_insert(e);
    }

    let mut items = Vec::new();
    for e in entries.iter().filter(|e| !e.is_raw) {
        let pair_id = e.pair_id();
        let stars = selection.ratings.get(pair_id).map(|r| r.stars);
        let burst = selection.bursts.get(pair_id);
        let analysis = analyzed.get(pair_id);

        if let Some(min_s) = filter.min_stars {
            if !stars.is_some_and(|s| s >= min_s) {
                continue;
            }
        }
        if let Some(allowed) = &filter.stars {
            if !stars.is_some_and(|s| allowed.contains(&s)) {
                continue;
            }
        }
        let group = burst.map(|b| b.group).unwrap_or(0);
        if filter.keep_only && group > 0 && !burst.is_some_and(|b| b.keep) {
            continue;
        }
        match filter.group_filter {
            GroupFilter::All => {}
            GroupFilter::BurstOnly => {
                if group == 0 {
                    continue;
                }
            }
            GroupFilter::SingleOnly => {
                if group != 0 {
                    continue;
                }
            }
            GroupFilter::Group(gid) => {
                if group != gid {
                    continue;
                }
            }
        }
        if filter.faces_only && !analysis.is_some_and(|a| a.faces > 0) {
            continue;
        }

        let jpg_rel = normalize_rel(root, Path::new(&e.path));
        let raw_rel = raw_by_pair
            .get(pair_id)
            .map(|r| normalize_rel(root, Path::new(&r.path)));
        let xmp_meta = analysis.map(|r| {
            let total = score::total_score(&r.scores, &cfg.weights);
            let mut xmp_entry = e.clone();
            xmp_entry.total_score = format!("{total:.1}");
            xmp_entry.faces = r.faces.to_string();
            if let Some(b) = burst {
                xmp_entry.burst_group = if b.group == 0 { "0".into() } else { b.group.to_string() };
                xmp_entry.burst_rank = if b.size == 0 { String::new() } else { b.rank.to_string() };
                xmp_entry.burst_keep = if b.size == 0 { String::new() } else { b.keep.to_string() };
            }
            ExportXmpMeta {
                entry: xmp_entry,
                scores: r.scores,
                total,
                rating: stars.unwrap_or(3),
                suggested_ev: r.suggested_ev,
            }
        });
        items.push(ExportPhotoItem {
            pair_id: pair_id.to_string(),
            jpg_rel: Some(jpg_rel),
            raw_rel,
            xmp_meta,
        });
    }

    Ok((items, all_source_rels))
}

/// 生成导出计划与预览摘要（只读，绝不在磁盘创建或修改任何目录/文件）。
pub fn plan_export(
    root: &Path,
    items: &[ExportPhotoItem],
    all_source_rels: &[String],
    opts: &ExportOptions,
) -> Result<ExportPlan> {
    anyhow::ensure!(
        opts.include_jpg || opts.include_raw || opts.include_xmp,
        "请至少勾选一种要导出的文件类型（JPG、RAW 或 XMP）"
    );

    let resolved_target = resolve_target(root, &opts.target, all_source_rels)?;

    let mut warnings = Vec::new();
    let target_folder = match resolved_target.open_folder(root, false) {
        Ok(folder) => Some(folder),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            return Err(err).with_context(|| {
                format!("导出目标不可用: {}", resolved_target.display_path().display())
            });
        }
    };

    let manifest = match &target_folder {
        Some(folder) => match read_manifest(folder) {
            Ok(m) => m,
            Err(err) => {
                warnings.push(format!("已有导出清单无法解析，将按保守保护模式检查既有文件: {err:#}"));
                ExportManifest::default()
            }
        },
        None => ExportManifest::default(),
    };

    let (stems, renamed_all) = assign_output_stems(items, opts.layout);

    let mut planned: Vec<PlannedFile> = Vec::new();
    let mut seen_dst: HashSet<String> = HashSet::new();
    let mut missing_raw_all: Vec<String> = Vec::new();
    let mut missing_xmp_unscored = 0usize;

    for (item, out_stem) in items.iter().zip(stems.iter()) {
        let label = item
            .jpg_rel
            .as_deref()
            .or(item.raw_rel.as_deref())
            .unwrap_or(&item.pair_id);

        if opts.include_raw && item.raw_rel.is_none() {
            missing_raw_all.push(label.to_string());
        }

        // 1) JPG
        if opts.include_jpg {
            if let Some(jpg_rel) = &item.jpg_rel {
                let dst_rel = photo_dst_rel(opts.layout, ExportFileKind::Jpg, jpg_rel, out_stem);
                if seen_dst.insert(dst_rel.to_ascii_lowercase()) {
                    let (src_size, src_mtime, src_mtime_nanos) = read_source_meta(root, jpg_rel)?;
                    let action = classify_photo_target(
                        target_folder.as_ref(),
                        &manifest,
                        &dst_rel,
                        jpg_rel,
                        src_size,
                        src_mtime_nanos,
                    )?;
                    planned.push(PlannedFile {
                        dst_rel,
                        source_identity: jpg_rel.clone(),
                        kind: ExportFileKind::Jpg,
                        source: PlannedSource::CopyPhoto {
                            src_rel: jpg_rel.clone(),
                            src_size,
                            src_mtime,
                        },
                        action,
                    });
                }
            }
        }

        // 2) RAW
        if opts.include_raw {
            if let Some(raw_rel) = &item.raw_rel {
                let dst_rel = photo_dst_rel(opts.layout, ExportFileKind::Raw, raw_rel, out_stem);
                if seen_dst.insert(dst_rel.to_ascii_lowercase()) {
                    let (src_size, src_mtime, src_mtime_nanos) = read_source_meta(root, raw_rel)?;
                    let action = classify_photo_target(
                        target_folder.as_ref(),
                        &manifest,
                        &dst_rel,
                        raw_rel,
                        src_size,
                        src_mtime_nanos,
                    )?;
                    planned.push(PlannedFile {
                        dst_rel,
                        source_identity: raw_rel.clone(),
                        kind: ExportFileKind::Raw,
                        source: PlannedSource::CopyPhoto {
                            src_rel: raw_rel.clone(),
                            src_size,
                            src_mtime,
                        },
                        action,
                    });
                }
            }
        }

        // 3) XMP（按布局决定写 1 份还是 JPG/RAW 两端各 1 份）
        if opts.include_xmp {
            let xmp_targets = xmp_dst_rels(opts, item, out_stem);
            if !xmp_targets.is_empty() {
                let mut unscored_counted = false;
                for (dst_rel, prefer_raw_side) in xmp_targets {
                    if !seen_dst.insert(dst_rel.to_ascii_lowercase()) {
                        continue;
                    }
                    let desired = resolve_xmp_bytes(
                        root,
                        item,
                        prefer_raw_side,
                        opts.xmp_exposure,
                    )?;
                    let Some((bytes, generated)) = desired else {
                        if !unscored_counted {
                            missing_xmp_unscored += 1;
                            unscored_counted = true;
                        }
                        continue;
                    };
                    let sha256 = sha256_hex(&bytes);
                    let source_identity = label.to_string();
                    let action = classify_xmp_target(
                        target_folder.as_ref(),
                        &manifest,
                        &dst_rel,
                        &source_identity,
                        &sha256,
                    )?;
                    planned.push(PlannedFile {
                        dst_rel,
                        source_identity,
                        kind: ExportFileKind::Xmp,
                        source: PlannedSource::WriteXmp {
                            bytes,
                            sha256,
                            generated,
                        },
                        action,
                    });
                }
            }
        }
    }

    let mut jpg_to_copy = 0usize;
    let mut raw_to_copy = 0usize;
    let mut xmp_to_write = 0usize;
    let mut xmp_generated = 0usize;
    let mut skipped_unchanged = 0usize;
    let mut protected_existing = 0usize;
    let mut total_copy_bytes = 0u64;
    let mut protected_samples = Vec::new();

    for f in &planned {
        match f.action {
            FileAction::Write => match (&f.kind, &f.source) {
                (ExportFileKind::Jpg, PlannedSource::CopyPhoto { src_size, .. }) => {
                    jpg_to_copy += 1;
                    total_copy_bytes = total_copy_bytes.saturating_add(*src_size);
                }
                (ExportFileKind::Raw, PlannedSource::CopyPhoto { src_size, .. }) => {
                    raw_to_copy += 1;
                    total_copy_bytes = total_copy_bytes.saturating_add(*src_size);
                }
                (ExportFileKind::Xmp, PlannedSource::WriteXmp { bytes, generated, .. }) => {
                    xmp_to_write += 1;
                    if *generated {
                        xmp_generated += 1;
                    }
                    total_copy_bytes = total_copy_bytes.saturating_add(bytes.len() as u64);
                }
                _ => {}
            },
            FileAction::SkipUnchanged => skipped_unchanged += 1,
            FileAction::ProtectExisting => {
                protected_existing += 1;
                if protected_samples.len() < 10 {
                    protected_samples.push(f.dst_rel.clone());
                }
            }
        }
    }

    let available_bytes = storage::available_disk_space(resolved_target.display_path());
    let low_disk_space = available_bytes
        .is_some_and(|free| free < total_copy_bytes.saturating_add(DISK_HEADROOM_BYTES));
    if low_disk_space {
        warnings.push("目标磁盘剩余空间可能不足，请清理空间或减少导出范围。".into());
    }
    if protected_existing > 0 {
        warnings.push(format!(
            "目标目录中有 {protected_existing} 个既有文件（可能已在 PS/ACR 修改）将被保留，不会覆盖。"
        ));
    }

    let preview = ExportPreview {
        target_display: resolved_target.display_path().display().to_string(),
        in_album: resolved_target.is_in_album(),
        selected_photos: items.len(),
        jpg_to_copy,
        raw_to_copy,
        xmp_to_write,
        xmp_generated,
        skipped_unchanged,
        protected_existing,
        total_copy_bytes,
        available_bytes,
        low_disk_space,
        missing_raw_count: missing_raw_all.len(),
        missing_raw_samples: missing_raw_all.into_iter().take(10).collect(),
        missing_xmp_unscored,
        renamed_count: renamed_all.len(),
        renamed_samples: renamed_all.into_iter().take(20).collect(),
        protected_samples,
        warnings,
    };

    Ok(ExportPlan {
        root: root.to_path_buf(),
        target: resolved_target,
        files: planned,
        preview,
    })
}

/// 执行导出计划（支持原子取消、实时进度与非破坏清单维护）。
pub fn run_export(
    plan: &ExportPlan,
    cancel: &AtomicBool,
    on_event: &(dyn Fn(ExportEvent) + Sync),
) -> Result<ExportSummary> {
    let target_folder = plan
        .target
        .open_folder(&plan.root, true)
        .with_context(|| format!("创建或打开导出目录失败: {}", plan.target.display_path().display()))?;

    let mut manifest = read_manifest(&target_folder).unwrap_or_default();
    manifest.version = 1;
    manifest.source_root = plan.root.display().to_string();
    manifest.updated_at_unix_ms = now_unix_ms();
    // 在复制任何照片之前先落盘 `.firstcut-export`，即使中途取消/崩溃也不会被上级扫描反吃。
    save_manifest(&target_folder, &manifest)?;

    let to_write: Vec<&PlannedFile> = plan
        .files
        .iter()
        .filter(|f| f.action == FileAction::Write)
        .collect();
    let total_files = to_write.len();
    let total_bytes = plan.preview.total_copy_bytes;

    on_event(ExportEvent::Info(format!(
        "开始导出到 {}：待写入 {} 个文件（跳过未变 {}，保留既有 {}）",
        plan.preview.target_display,
        total_files,
        plan.preview.skipped_unchanged,
        plan.preview.protected_existing,
    )));

    let mut jpg_copied = 0usize;
    let mut raw_copied = 0usize;
    let mut xmp_written = 0usize;
    let mut bytes_written = 0u64;
    let mut done_files = 0usize;
    let mut cancelled = false;
    let mut errors = Vec::new();

    for planned in to_write {
        if cancel.load(Ordering::Acquire) {
            cancelled = true;
            on_event(ExportEvent::Info("收到取消请求，已完成当前文件并安全停止导出。".into()));
            break;
        }
        on_event(ExportEvent::Progress {
            done: done_files,
            total: total_files,
            bytes_done: bytes_written,
            bytes_total: total_bytes,
            current_file: planned.dst_rel.clone(),
        });

        let write_result: Result<(u64, Option<i64>, Option<String>)> = (|| {
            let dst_ref = open_target_file_ref(&target_folder, &planned.dst_rel, true)?;
            match &planned.source {
                PlannedSource::CopyPhoto {
                    src_rel,
                    src_mtime,
                    ..
                } => {
                    let src_full = plan.root.join(src_rel);
                    let mut src_file = FileRef::inside(&plan.root, &src_full)?
                        .open_read()?
                        .ok_or_else(|| anyhow!("源文件不存在: {src_rel}"))?;
                    let (written, actual_mtime) =
                        dst_ref.copy_from_reader(&mut src_file, *src_mtime)?;
                    Ok((written, actual_mtime, None))
                }
                PlannedSource::WriteXmp { bytes, sha256, .. } => {
                    let (written, actual_mtime) = dst_ref.write_exported_bytes(bytes)?;
                    Ok((written, actual_mtime, Some(sha256.clone())))
                }
            }
        })();

        match write_result {
            Ok((written, actual_mtime, sha256)) => {
                done_files += 1;
                bytes_written = bytes_written.saturating_add(written);
                match planned.kind {
                    ExportFileKind::Jpg => jpg_copied += 1,
                    ExportFileKind::Raw => raw_copied += 1,
                    ExportFileKind::Xmp => xmp_written += 1,
                }
                manifest.files.insert(
                    planned.dst_rel.clone(),
                    ManifestEntry {
                        size: written,
                        mtime_nanos: actual_mtime.unwrap_or(0),
                        kind: planned.kind.as_str().to_string(),
                        sha256,
                        source_rel: Some(planned.source_identity.clone()),
                    },
                );
                if done_files % 25 == 0 {
                    manifest.updated_at_unix_ms = now_unix_ms();
                    let _ = save_manifest(&target_folder, &manifest);
                }
            }
            Err(err) => {
                let msg = format!("{} 写入失败: {err:#}", planned.dst_rel);
                on_event(ExportEvent::Info(format!("[export] {msg}")));
                errors.push(msg);
            }
        }
    }

    manifest.updated_at_unix_ms = now_unix_ms();
    save_manifest(&target_folder, &manifest)?;

    on_event(ExportEvent::Progress {
        done: done_files,
        total: total_files,
        bytes_done: bytes_written,
        bytes_total: total_bytes,
        current_file: String::new(),
    });

    Ok(ExportSummary {
        target_display: plan.preview.target_display.clone(),
        target_path: plan.target.display_path().to_path_buf(),
        in_album: plan.target.is_in_album(),
        jpg_copied,
        raw_copied,
        xmp_written,
        skipped_unchanged: plan.preview.skipped_unchanged,
        protected_existing: plan.preview.protected_existing,
        failed: errors.len(),
        bytes_written,
        cancelled,
        errors,
    })
}

/// 校验并归一化导出目标路径；同时防止导出目标覆盖原片目录或 `.firstcut/`。
fn resolve_target(
    root: &Path,
    target: &ExportTarget,
    all_source_rels: &[String],
) -> Result<ResolvedTarget> {
    match target {
        ExportTarget::AlbumSubdir(rel) => validate_album_subdir(root, rel, all_source_rels),
        ExportTarget::ExternalDir(path) => {
            let trimmed = path.to_string_lossy().trim().to_string();
            anyhow::ensure!(!trimmed.is_empty(), "导出目录不能为空");
            let abs = std::path::absolute(Path::new(&trimmed))
                .with_context(|| format!("无法解析导出路径: {}", path.display()))?;
            if let Some(rel_under_album) = storage::relative_under_album(root, &abs)? {
                if rel_under_album.as_os_str().is_empty() {
                    bail!("导出目录不能是照片根目录本身，请选择或输入独立子目录（如 Selected）");
                }
                return validate_album_subdir(root, &rel_under_album, all_source_rels);
            }
            if storage::relative_under_album(&abs, root)?.is_some() {
                bail!("导出目录不能是当前照片根目录的上级目录");
            }
            Ok(ResolvedTarget::ExternalDir { path: abs })
        }
    }
}

fn validate_album_subdir(
    root: &Path,
    raw_rel: &Path,
    all_source_rels: &[String],
) -> Result<ResolvedTarget> {
    let raw_str = raw_rel.to_string_lossy().trim().replace('\\', "/");
    anyhow::ensure!(!raw_str.is_empty(), "导出子目录名称不能为空");
    let rel_path = Path::new(&raw_str);
    if rel_path.has_root() {
        bail!("相册内导出子目录必须是相对路径（例如 Selected）");
    }
    let mut clean_parts: Vec<String> = Vec::new();
    for comp in rel_path.components() {
        match comp {
            Component::Normal(seg) => {
                let s = seg.to_string_lossy().trim().to_string();
                anyhow::ensure!(!s.is_empty(), "导出子目录路径包含空白段");
                clean_parts.push(s);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => {
                bail!("相册内导出子目录不能包含 `..`、盘符或根路径");
            }
        }
    }
    anyhow::ensure!(
        !clean_parts.is_empty(),
        "导出目录不能是照片根目录本身，请输入独立子目录（如 Selected）"
    );
    if clean_parts[0].eq_ignore_ascii_case(".firstcut") {
        bail!("不能导出到 .firstcut 内部状态目录");
    }

    // 防止把现有原片目录（如 JPG/ 或 RAW/）当成导出目录，导致写入 .firstcut-export 后下次扫描跳过原片
    let target_norm = clean_parts
        .iter()
        .map(|s| s.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join("/");
    for src in all_source_rels {
        let src_lower = src.replace('\\', "/").to_ascii_lowercase();
        let prefix = format!("{target_norm}/");
        if src_lower.starts_with(&prefix) {
            bail!(
                "导出目录「{}」内已包含原始照片，不能将其作为导出目录；请改用独立子目录（如 Selected）",
                clean_parts.join("/")
            );
        }
    }

    let mut clean_rel = PathBuf::new();
    for part in &clean_parts {
        clean_rel.push(part);
    }
    let display = root.join(&clean_rel);
    Ok(ResolvedTarget::AlbumSubdir {
        rel: clean_rel,
        display,
    })
}

/// 为每张待导出照片分配冲突安全的主干名（同一张照片的 JPG/RAW/XMP 共享同一主干）。
fn assign_output_stems(
    items: &[ExportPhotoItem],
    layout: ExportLayout,
) -> (Vec<String>, Vec<CollisionNote>) {
    let base_stems: Vec<String> = items
        .iter()
        .map(|item| {
            let rel = item
                .jpg_rel
                .as_deref()
                .or(item.raw_rel.as_deref())
                .unwrap_or(&item.pair_id);
            let fname = Path::new(rel)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(rel);
            scan::stem_raw_of(fname)
        })
        .collect();

    if layout == ExportLayout::PreserveTree {
        return (base_stems, Vec::new());
    }

    let mut freq: HashMap<String, usize> = HashMap::new();
    for stem in &base_stems {
        *freq.entry(stem.to_ascii_lowercase()).or_insert(0) += 1;
    }

    let mut used: HashSet<String> = HashSet::new();
    // 先占位无冲突主干，避免重命名后的结果撞到其他无冲突主干
    for stem in &base_stems {
        let lower = stem.to_ascii_lowercase();
        if freq.get(&lower).copied().unwrap_or(0) == 1 {
            used.insert(lower);
        }
    }

    let mut out = Vec::with_capacity(items.len());
    let mut collisions = Vec::new();
    for (item, stem) in items.iter().zip(base_stems.iter()) {
        let lower = stem.to_ascii_lowercase();
        if freq.get(&lower).copied().unwrap_or(0) <= 1 {
            out.push(stem.clone());
            continue;
        }
        let source_rel = item
            .jpg_rel
            .as_deref()
            .or(item.raw_rel.as_deref())
            .unwrap_or(&item.pair_id);
        let parent = Path::new(source_rel)
            .parent()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let prefix = sanitize_prefix(&parent);
        let base_candidate = format!("{prefix}__{stem}");
        let mut candidate = base_candidate.clone();
        let mut suffix = 2usize;
        while !used.insert(candidate.to_ascii_lowercase()) {
            candidate = format!("{base_candidate}_{suffix}");
            suffix += 1;
        }
        collisions.push(CollisionNote {
            source: source_rel.to_string(),
            exported_stem: candidate.clone(),
        });
        out.push(candidate);
    }

    (out, collisions)
}

fn sanitize_prefix(parent: &str) -> String {
    let cleaned: String = parent
        .trim_matches(['/', '\\', ' ', '.'])
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    if cleaned.is_empty() {
        "root".to_string()
    } else {
        cleaned
    }
}

fn photo_dst_rel(layout: ExportLayout, kind: ExportFileKind, src_rel: &str, out_stem: &str) -> String {
    let ext = Path::new(src_rel)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or(match kind {
            ExportFileKind::Jpg => "JPG",
            ExportFileKind::Raw => "ARW",
            ExportFileKind::Xmp => "xmp",
        });
    match layout {
        ExportLayout::SplitByType => {
            let folder = match kind {
                ExportFileKind::Jpg => "JPG",
                ExportFileKind::Raw => "RAW",
                ExportFileKind::Xmp => "",
            };
            if folder.is_empty() {
                format!("{out_stem}.{ext}")
            } else {
                format!("{folder}/{out_stem}.{ext}")
            }
        }
        ExportLayout::Flat => format!("{out_stem}.{ext}"),
        ExportLayout::PreserveTree => src_rel.replace('\\', "/"),
    }
}

/// 返回需要写出的 `(dst_rel, prefer_raw_side)` 列表
fn xmp_dst_rels(
    opts: &ExportOptions,
    item: &ExportPhotoItem,
    out_stem: &str,
) -> Vec<(String, bool)> {
    match opts.layout {
        ExportLayout::SplitByType => {
            let mut out = Vec::new();
            if opts.include_jpg && item.jpg_rel.is_some() {
                out.push((format!("JPG/{out_stem}.xmp"), false));
            }
            if opts.include_raw && item.raw_rel.is_some() {
                out.push((format!("RAW/{out_stem}.xmp"), true));
            }
            if !opts.include_jpg && !opts.include_raw {
                out.push((format!("{out_stem}.xmp"), item.raw_rel.is_some()));
            }
            out
        }
        ExportLayout::Flat => {
            vec![(format!("{out_stem}.xmp"), item.raw_rel.is_some())]
        }
        ExportLayout::PreserveTree => {
            let mut out = Vec::new();
            if opts.include_jpg {
                if let Some(jpg_rel) = &item.jpg_rel {
                    out.push((sibling_xmp_rel(jpg_rel), false));
                }
            }
            if opts.include_raw {
                if let Some(raw_rel) = &item.raw_rel {
                    out.push((sibling_xmp_rel(raw_rel), true));
                }
            }
            if !opts.include_jpg && !opts.include_raw {
                if let Some(rel) = item.raw_rel.as_deref().or(item.jpg_rel.as_deref()) {
                    out.push((sibling_xmp_rel(rel), item.raw_rel.is_some()));
                }
            }
            out
        }
    }
}

fn sibling_xmp_rel(photo_rel: &str) -> String {
    let p = Path::new(photo_rel);
    let fname = p.file_name().and_then(|s| s.to_str()).unwrap_or(photo_rel);
    let stem = scan::stem_raw_of(fname);
    p.with_file_name(format!("{stem}.xmp"))
        .to_string_lossy()
        .replace('\\', "/")
}

/// 决定待写出的 XMP 字节内容：优先原样复制已有侧车；若尚无侧车且已评分，则按方案 A 生成新侧车。
fn resolve_xmp_bytes(
    root: &Path,
    item: &ExportPhotoItem,
    prefer_raw_side: bool,
    write_crs: bool,
) -> Result<Option<(Vec<u8>, bool)>> {
    let candidates: Vec<String> = if prefer_raw_side {
        [item.raw_rel.as_deref(), item.jpg_rel.as_deref()]
            .into_iter()
            .flatten()
            .map(sibling_xmp_rel)
            .collect()
    } else {
        [item.jpg_rel.as_deref(), item.raw_rel.as_deref()]
            .into_iter()
            .flatten()
            .map(sibling_xmp_rel)
            .collect()
    };

    for xmp_rel in candidates {
        let full = root.join(&xmp_rel);
        if let Some(bytes) = FileRef::inside(root, &full)?.read(MAX_XMP_BYTES)? {
            return Ok(Some((bytes, false)));
        }
    }

    let Some(meta) = &item.xmp_meta else {
        return Ok(None);
    };
    let xml = crate::output::xmp::render_xmp_with_crs(
        &meta.entry,
        &meta.scores,
        meta.total,
        meta.rating,
        meta.suggested_ev,
        write_crs,
    );
    Ok(Some((xml.into_bytes(), true)))
}

fn read_source_meta(root: &Path, src_rel: &str) -> Result<(u64, Option<SystemTime>, i64)> {
    let full = root.join(src_rel);
    let file = FileRef::inside(root, &full)?
        .open_read()?
        .ok_or_else(|| anyhow!("源照片不存在: {src_rel}"))?;
    let meta = file.metadata()?;
    let mtime = meta.modified().ok();
    let (size, mtime_nanos) = cache::metadata_fingerprint(&meta)
        .ok_or_else(|| anyhow!("无法读取源照片时间信息: {src_rel}"))?;
    Ok((size, mtime, mtime_nanos))
}

fn open_target_file_ref(target_root: &Folder, dst_rel: &str, create_parent: bool) -> std::io::Result<FileRef> {
    let rel = Path::new(dst_rel);
    let name = rel
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "缺少目标文件名"))?;
    let parent = rel.parent().unwrap_or_else(|| Path::new(""));
    if parent.as_os_str().is_empty() {
        target_root.file(name)
    } else {
        target_root.descend_cloned(parent, create_parent)?.file(name)
    }
}

fn classify_photo_target(
    target_folder: Option<&Folder>,
    manifest: &ExportManifest,
    dst_rel: &str,
    src_rel: &str,
    src_size: u64,
    src_mtime_nanos: i64,
) -> Result<FileAction> {
    let Some(folder) = target_folder else {
        return Ok(FileAction::Write);
    };
    let dst_ref = match open_target_file_ref(folder, dst_rel, false) {
        Ok(r) => r,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(FileAction::Write),
        Err(err) => return Err(err.into()),
    };
    let Some((dst_size, dst_mtime)) = dst_ref.fingerprint()? else {
        return Ok(FileAction::Write);
    };
    if let Some(recorded) = manifest.files.get(dst_rel) {
        // 如果清单记录该目标路径来自另一张不同源照片，绝不静默覆盖
        if let Some(recorded_src) = recorded.source_rel.as_deref() {
            if !recorded_src.eq_ignore_ascii_case(src_rel) {
                return Ok(FileAction::ProtectExisting);
            }
        }
        if recorded.size == dst_size && recorded.mtime_nanos == dst_mtime {
            if dst_size == src_size && (dst_mtime - src_mtime_nanos).abs() <= 2_000_000_000 {
                return Ok(FileAction::SkipUnchanged);
            }
            return Ok(FileAction::Write);
        }
    }
    // 未在清单里但大小与修改时间完全一致（≤ 1ms）：增量跳过
    let mtime_close = (dst_mtime - src_mtime_nanos).abs() <= 1_000_000;
    if dst_size == src_size && mtime_close {
        return Ok(FileAction::SkipUnchanged);
    }
    Ok(FileAction::ProtectExisting)
}

fn classify_xmp_target(
    target_folder: Option<&Folder>,
    manifest: &ExportManifest,
    dst_rel: &str,
    source_identity: &str,
    desired_sha256: &str,
) -> Result<FileAction> {
    let Some(folder) = target_folder else {
        return Ok(FileAction::Write);
    };
    let dst_ref = match open_target_file_ref(folder, dst_rel, false) {
        Ok(r) => r,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(FileAction::Write),
        Err(err) => return Err(err.into()),
    };
    let Some(existing_bytes) = dst_ref.read(MAX_XMP_BYTES)? else {
        return Ok(FileAction::Write);
    };
    let existing_sha = sha256_hex(&existing_bytes);
    if existing_sha == desired_sha256 {
        return Ok(FileAction::SkipUnchanged);
    }
    // 仅当目标 XMP 来自同一源照片且内容哈希与上次 firstcut 写出记录一致（用户未在 PS/ACR 改过）时才更新
    if let Some(recorded) = manifest.files.get(dst_rel) {
        let same_source = recorded
            .source_rel
            .as_deref()
            .is_none_or(|s| s.eq_ignore_ascii_case(source_identity));
        if same_source && recorded.sha256.as_deref() == Some(existing_sha.as_str()) {
            return Ok(FileAction::Write);
        }
    }
    Ok(FileAction::ProtectExisting)
}

fn read_manifest(target_folder: &Folder) -> Result<ExportManifest> {
    let Some(bytes) = target_folder.file(EXPORT_MARKER_NAME)?.read(MAX_MANIFEST_BYTES)? else {
        return Ok(ExportManifest::default());
    };
    let manifest: ExportManifest = serde_json::from_slice(&bytes)
        .with_context(|| format!("解析 {EXPORT_MARKER_NAME} 失败"))?;
    Ok(manifest)
}

fn save_manifest(target_folder: &Folder, manifest: &ExportManifest) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(manifest)?;
    target_folder.file(EXPORT_MARKER_NAME)?.write(&bytes)?;
    Ok(())
}

fn normalize_rel(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path).to_string_lossy();
    #[cfg(windows)]
    {
        rel.replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        rel.into_owned()
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_meta(root: &Path, rel: &str, rating: u8) -> ExportXmpMeta {
        let full = root.join(rel);
        let filename = full.file_name().unwrap().to_string_lossy().to_string();
        ExportXmpMeta {
            entry: PhotoEntry {
                path: full.display().to_string(),
                filename,
                total_score: "80.0".into(),
                faces: "1".into(),
                burst_group: "0".into(),
                ..Default::default()
            },
            scores: PixelScores {
                sharpness: 80.0,
                exposure: 80.0,
                noise: 80.0,
                composition: 80.0,
                aesthetic: 80.0,
            },
            total: 80.0,
            rating,
            suggested_ev: Some(0.33),
        }
    }

    #[test]
    fn split_layout_exports_jpg_raw_and_generated_xmp_and_protects_ps_edits() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("album");
        std::fs::create_dir_all(root.join("JPG")).unwrap();
        std::fs::create_dir_all(root.join("RAW")).unwrap();
        std::fs::write(root.join("JPG/DSC00001.JPG"), b"jpg-bytes-1").unwrap();
        std::fs::write(root.join("RAW/DSC00001.ARW"), b"raw-bytes-1").unwrap();
        std::fs::write(root.join("JPG/DSC00002.JPG"), b"jpg-only").unwrap();

        let all_sources = vec![
            "JPG/DSC00001.JPG".into(),
            "RAW/DSC00001.ARW".into(),
            "JPG/DSC00002.JPG".into(),
        ];
        let items = vec![
            ExportPhotoItem {
                pair_id: "p1".into(),
                jpg_rel: Some("JPG/DSC00001.JPG".into()),
                raw_rel: Some("RAW/DSC00001.ARW".into()),
                xmp_meta: Some(sample_meta(&root, "JPG/DSC00001.JPG", 5)),
            },
            ExportPhotoItem {
                pair_id: "p2".into(),
                jpg_rel: Some("JPG/DSC00002.JPG".into()),
                raw_rel: None,
                xmp_meta: Some(sample_meta(&root, "JPG/DSC00002.JPG", 4)),
            },
        ];

        let opts = ExportOptions::default();
        let plan = plan_export(&root, &items, &all_sources, &opts).unwrap();
        assert_eq!(plan.preview.jpg_to_copy, 2);
        assert_eq!(plan.preview.raw_to_copy, 1);
        assert_eq!(plan.preview.xmp_to_write, 3, "SplitByType 两端各自生成 XMP");
        assert_eq!(plan.preview.xmp_generated, 3);
        assert_eq!(plan.preview.missing_raw_count, 1);
        assert!(!root.join("Selected").exists(), "预览阶段绝不能创建目标目录");

        let cancel = AtomicBool::new(false);
        let summary = run_export(&plan, &cancel, &|_| {}).unwrap();
        assert_eq!(summary.jpg_copied, 2);
        assert_eq!(summary.raw_copied, 1);
        assert_eq!(summary.xmp_written, 3);
        assert!(!summary.cancelled);
        assert!(root.join("Selected").join(EXPORT_MARKER_NAME).is_file());
        // 源目录不应被写入 XMP
        assert!(!root.join("JPG/DSC00001.xmp").exists());
        assert!(!root.join("RAW/DSC00001.xmp").exists());
        // 生成的 XMP 遵守方案 A：有 firstcut:suggestedEV，无 crs:Exposure2012
        let raw_xmp = std::fs::read_to_string(root.join("Selected/RAW/DSC00001.xmp")).unwrap();
        assert!(raw_xmp.contains("<xmp:Rating>5</xmp:Rating>"));
        assert!(raw_xmp.contains("<firstcut:suggestedEV>+0.33</firstcut:suggestedEV>"));
        assert!(!raw_xmp.contains("crs:Exposure2012"));

        // 模拟用户在导出目录里放入 PSD 并用 PS/ACR 编辑了 RAW/DSC00001.xmp
        std::fs::write(root.join("Selected/RAW/DSC00001.psd"), b"my-retouch").unwrap();
        let edited_xmp = raw_xmp.replace(
            "xmlns:firstcut=\"http://firstcut.local/ns/\">",
            "xmlns:firstcut=\"http://firstcut.local/ns/\" xmlns:crs=\"http://ns.adobe.com/camera-raw-settings/1.0/\" crs:Exposure2012=\"+1.50\">",
        );
        std::fs::write(root.join("Selected/RAW/DSC00001.xmp"), &edited_xmp).unwrap();

        // 改星为 3★ 后二次导出：JPG/RAW 增量跳过，未改过的 JPG/DSC00001.xmp 同步更新为 3★，
        // 已被 PS 改过的 RAW/DSC00001.xmp 保留不覆盖，PSD 原封不动
        let mut items2 = items.clone();
        items2[0].xmp_meta = Some(sample_meta(&root, "JPG/DSC00001.JPG", 3));
        let plan2 = plan_export(&root, &items2, &all_sources, &opts).unwrap();
        assert_eq!(plan2.preview.jpg_to_copy, 0, "未变化的 JPG 应增量跳过");
        assert_eq!(plan2.preview.raw_to_copy, 0, "未变化的 RAW 应增量跳过");
        assert_eq!(plan2.preview.xmp_to_write, 1, "仅未被动过的 JPG/DSC00001.xmp 需要更新");
        assert_eq!(plan2.preview.protected_existing, 1, "PS 改过的 RAW/DSC00001.xmp 应受保护");
        let summary2 = run_export(&plan2, &cancel, &|_| {}).unwrap();
        assert_eq!(summary2.xmp_written, 1);
        assert_eq!(summary2.protected_existing, 1);
        assert!(std::fs::read_to_string(root.join("Selected/JPG/DSC00001.xmp"))
            .unwrap()
            .contains("<xmp:Rating>3</xmp:Rating>"));
        assert!(std::fs::read_to_string(root.join("Selected/RAW/DSC00001.xmp"))
            .unwrap()
            .contains("crs:Exposure2012=\"+1.50\""));
        assert_eq!(std::fs::read(root.join("Selected/RAW/DSC00001.psd")).unwrap(), b"my-retouch");

        // 上级相册再次扫描不会重复扫入 Selected/
        let rescanned = scan::scan_directory(&root).unwrap();
        assert_eq!(rescanned.len(), 3);
    }

    #[test]
    fn rollover_collisions_rename_consistently_across_jpg_raw_and_xmp() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("album");
        std::fs::create_dir_all(root.join("Day1")).unwrap();
        std::fs::create_dir_all(root.join("Day2")).unwrap();
        std::fs::write(root.join("Day1/DSC00001.JPG"), b"d1-jpg").unwrap();
        std::fs::write(root.join("Day1/DSC00001.ARW"), b"d1-raw").unwrap();
        std::fs::write(root.join("Day2/DSC00001.JPG"), b"d2-jpg").unwrap();
        std::fs::write(root.join("Day2/DSC00001.ARW"), b"d2-raw").unwrap();

        let all_sources = vec![
            "Day1/DSC00001.JPG".into(),
            "Day1/DSC00001.ARW".into(),
            "Day2/DSC00001.JPG".into(),
            "Day2/DSC00001.ARW".into(),
        ];
        let items = vec![
            ExportPhotoItem {
                pair_id: "p1".into(),
                jpg_rel: Some("Day1/DSC00001.JPG".into()),
                raw_rel: Some("Day1/DSC00001.ARW".into()),
                xmp_meta: Some(sample_meta(&root, "Day1/DSC00001.JPG", 5)),
            },
            ExportPhotoItem {
                pair_id: "p2".into(),
                jpg_rel: Some("Day2/DSC00001.JPG".into()),
                raw_rel: Some("Day2/DSC00001.ARW".into()),
                xmp_meta: Some(sample_meta(&root, "Day2/DSC00001.JPG", 4)),
            },
        ];

        let opts = ExportOptions {
            layout: ExportLayout::Flat,
            ..Default::default()
        };
        let plan = plan_export(&root, &items, &all_sources, &opts).unwrap();
        assert_eq!(plan.preview.renamed_count, 2);
        let summary = run_export(&plan, &AtomicBool::new(false), &|_| {}).unwrap();
        assert_eq!(summary.jpg_copied, 2);
        assert_eq!(summary.raw_copied, 2);
        assert_eq!(summary.xmp_written, 2);
        assert_eq!(std::fs::read(root.join("Selected/Day1__DSC00001.JPG")).unwrap(), b"d1-jpg");
        assert_eq!(std::fs::read(root.join("Selected/Day1__DSC00001.ARW")).unwrap(), b"d1-raw");
        assert!(root.join("Selected/Day1__DSC00001.xmp").is_file());
        assert_eq!(std::fs::read(root.join("Selected/Day2__DSC00001.JPG")).unwrap(), b"d2-jpg");
        assert_eq!(std::fs::read(root.join("Selected/Day2__DSC00001.ARW")).unwrap(), b"d2-raw");
        assert!(root.join("Selected/Day2__DSC00001.xmp").is_file());
    }

    #[test]
    fn target_validation_rejects_unsafe_paths_and_source_dirs() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("album");
        std::fs::create_dir_all(root.join("JPG")).unwrap();
        std::fs::write(root.join("JPG/a.jpg"), b"x").unwrap();
        let sources = vec!["JPG/a.jpg".into()];

        for bad in ["", ".", "..", "../escape", ".firstcut", ".firstcut/sub", "JPG", "jpg"] {
            let err = resolve_target(&root, &ExportTarget::AlbumSubdir(PathBuf::from(bad)), &sources);
            assert!(err.is_err(), "应拒绝非法子目录: {bad}");
        }
        // 外部路径不能指向相册根或相册上级
        assert!(resolve_target(&root, &ExportTarget::ExternalDir(root.clone()), &sources).is_err());
        assert!(resolve_target(&root, &ExportTarget::ExternalDir(temp.path().to_path_buf()), &sources).is_err());
        // 外部路径若指向独立外部目录则放行
        let outside = temp.path().join("export_outside");
        assert!(resolve_target(&root, &ExportTarget::ExternalDir(outside), &sources).is_ok());
    }

    #[test]
    fn cancel_stops_cleanly_without_partial_temp_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("album");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.jpg"), b"aaa").unwrap();
        std::fs::write(root.join("b.jpg"), b"bbb").unwrap();
        let items = vec![
            ExportPhotoItem {
                pair_id: "a".into(),
                jpg_rel: Some("a.jpg".into()),
                raw_rel: None,
                xmp_meta: None,
            },
            ExportPhotoItem {
                pair_id: "b".into(),
                jpg_rel: Some("b.jpg".into()),
                raw_rel: None,
                xmp_meta: None,
            },
        ];
        let opts = ExportOptions {
            include_raw: false,
            include_xmp: false,
            ..Default::default()
        };
        let plan = plan_export(&root, &items, &["a.jpg".into(), "b.jpg".into()], &opts).unwrap();
        let cancel = AtomicBool::new(false);
        let summary = run_export(&plan, &cancel, &|ev| {
            if let ExportEvent::Progress { done, .. } = ev {
                if done == 0 {
                    // 复制完第一个文件后触发取消
                    cancel.store(true, Ordering::Release);
                }
            }
        })
        .unwrap();
        assert!(summary.cancelled);
        assert_eq!(summary.jpg_copied, 1);
        assert!(root.join("Selected").join(EXPORT_MARKER_NAME).is_file());
        // 确认没有残留 .tmp 文件
        for entry in walkdir::WalkDir::new(root.join("Selected")) {
            let name = entry.unwrap().file_name().to_string_lossy().to_string();
            assert!(!name.ends_with(".tmp"), "不应残留临时文件: {name}");
        }
    }

    #[test]
    fn separate_runs_do_not_overwrite_different_source_with_same_filename() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("album");
        std::fs::create_dir_all(root.join("Day1")).unwrap();
        std::fs::create_dir_all(root.join("Day2")).unwrap();
        std::fs::write(root.join("Day1/DSC00001.JPG"), b"day1-original").unwrap();
        std::fs::write(root.join("Day2/DSC00001.JPG"), b"day2-different-photo").unwrap();
        let sources = vec!["Day1/DSC00001.JPG".into(), "Day2/DSC00001.JPG".into()];
        let opts = ExportOptions {
            layout: ExportLayout::Flat,
            include_raw: false,
            include_xmp: true,
            ..Default::default()
        };

        // 第一次只导出 Day1/DSC00001.JPG
        let item1 = vec![ExportPhotoItem {
            pair_id: "d1".into(),
            jpg_rel: Some("Day1/DSC00001.JPG".into()),
            raw_rel: None,
            xmp_meta: Some(sample_meta(&root, "Day1/DSC00001.JPG", 5)),
        }];
        let plan1 = plan_export(&root, &item1, &sources, &opts).unwrap();
        run_export(&plan1, &AtomicBool::new(false), &|_| {}).unwrap();

        // 第二次只导出同名的另一张照片 Day2/DSC00001.JPG：不能静默覆盖 Day1 的副本和 XMP
        let item2 = vec![ExportPhotoItem {
            pair_id: "d2".into(),
            jpg_rel: Some("Day2/DSC00001.JPG".into()),
            raw_rel: None,
            xmp_meta: Some(sample_meta(&root, "Day2/DSC00001.JPG", 3)),
        }];
        let plan2 = plan_export(&root, &item2, &sources, &opts).unwrap();
        assert_eq!(plan2.preview.jpg_to_copy, 0);
        assert_eq!(plan2.preview.xmp_to_write, 0);
        assert_eq!(plan2.preview.protected_existing, 2, "跨批次同名不同源照片与 XMP 均须保护");
        run_export(&plan2, &AtomicBool::new(false), &|_| {}).unwrap();
        assert_eq!(std::fs::read(root.join("Selected/DSC00001.JPG")).unwrap(), b"day1-original");
        assert!(std::fs::read_to_string(root.join("Selected/DSC00001.xmp"))
            .unwrap()
            .contains("<xmp:Rating>5</xmp:Rating>"));
    }
}
