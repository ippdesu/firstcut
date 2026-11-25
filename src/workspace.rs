//! 本地工作区模型、状态目录管理（源目录零写入）与最近工作区历史。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::storage::{self, Database, FileRef, Folder};

const MAX_RECENT_WORKSPACES: usize = 10;
const MAX_STATE_IMPORT_BYTES: u64 = 128 * 1024 * 1024;
const MAX_SMALL_CONFIG_BYTES: u64 = 16 * 1024 * 1024;

/// 照片工作区来源：单目录（相册 / 单卡）或双目录（JPG + RAW 双卡直连）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum WorkspaceSource {
    Single {
        root: PathBuf,
    },
    Dual {
        jpg_root: PathBuf,
        raw_root: PathBuf,
    },
}

impl WorkspaceSource {
    pub fn single(root: &Path) -> Result<Self> {
        let norm = normal_root(root)?;
        Ok(Self::Single { root: norm })
    }

    pub fn dual(jpg_root: &Path, raw_root: &Path) -> Result<Self> {
        let jpg_norm = normal_root(jpg_root)
            .with_context(|| format!("JPG 目录不可用: {}", jpg_root.display()))?;
        let raw_norm = normal_root(raw_root)
            .with_context(|| format!("RAW 目录不可用: {}", raw_root.display()))?;
        if storage::relative_under_album(&jpg_norm, &raw_norm)?.is_some()
            || storage::relative_under_album(&raw_norm, &jpg_norm)?.is_some()
        {
            bail!(
                "JPG 目录与 RAW 目录不能相同或互相包含；如果它们位于同一文件夹内，请改用「单目录」模式"
            );
        }
        Ok(Self::Dual {
            jpg_root: jpg_norm,
            raw_root: raw_norm,
        })
    }

    pub fn jpg_root(&self) -> &Path {
        match self {
            Self::Single { root } => root,
            Self::Dual { jpg_root, .. } => jpg_root,
        }
    }

    pub fn raw_root(&self) -> &Path {
        match self {
            Self::Single { root } => root,
            Self::Dual { raw_root, .. } => raw_root,
        }
    }

    pub fn is_dual(&self) -> bool {
        matches!(self, Self::Dual { .. })
    }

    pub fn mode_str(&self) -> &'static str {
        match self {
            Self::Single { .. } => "single",
            Self::Dual { .. } => "dual",
        }
    }

    pub fn display_label(&self) -> String {
        match self {
            Self::Single { root } => root.display().to_string(),
            Self::Dual { jpg_root, raw_root } => {
                format!("JPG: {} | RAW: {}", jpg_root.display(), raw_root.display())
            }
        }
    }

    /// 稳定的 16 位十六进制工作区哈希（Windows 路径大小写不敏感）。
    ///
    /// 若设置 `FIRSTCUT_WORKSPACE_KEY`（例如 Docker Compose 传入宿主机 `PHOTO_DIR`），
    /// 则将其一并纳入哈希，使不同宿主机相册即使挂载到相同的容器路径 `/photos` 也能严格隔离。
    pub fn workspace_id(&self) -> String {
        let key = workspace_key_override();
        self.workspace_id_with_key(key.as_deref())
    }

    pub fn workspace_id_with_key(&self, key_override: Option<&str>) -> String {
        let mut hasher = Sha256::new();
        if let Some(key) = key_override.and_then(normalize_workspace_key) {
            hasher.update(b"key\0");
            hasher.update(key.as_bytes());
            hasher.update(b"\0");
        }
        match self {
            Self::Single { root } => {
                hasher.update(b"single\0");
                hasher.update(canonical_key_str(root).as_bytes());
            }
            Self::Dual { jpg_root, raw_root } => {
                hasher.update(b"dual\0");
                hasher.update(canonical_key_str(jpg_root).as_bytes());
                hasher.update(b"\0");
                hasher.update(canonical_key_str(raw_root).as_bytes());
            }
        }
        let digest = hasher.finalize();
        let mut out = String::with_capacity(16);
        for byte in &digest[..8] {
            use std::fmt::Write;
            let _ = write!(&mut out, "{byte:02x}");
        }
        out
    }

    /// 相对所属源根目录的规范化路径（正斜杠；单目录相对 `root`，双目录分别相对 `jpg_root`/`raw_root`）。
    pub fn photo_rel(&self, is_raw: bool, full_path: &Path) -> String {
        let base = if is_raw { self.raw_root() } else { self.jpg_root() };
        let rel = full_path.strip_prefix(base).unwrap_or(full_path).to_string_lossy();
        #[cfg(windows)]
        {
            rel.replace('\\', "/")
        }
        #[cfg(not(windows))]
        {
            rel.into_owned()
        }
    }

    pub fn photo_full(&self, is_raw: bool, rel: &str) -> PathBuf {
        let base = if is_raw { self.raw_root() } else { self.jpg_root() };
        base.join(rel)
    }

    pub fn file_ref_for_photo(&self, is_raw: bool, full_path: &Path) -> std::io::Result<FileRef> {
        let base = if is_raw { self.raw_root() } else { self.jpg_root() };
        FileRef::inside(base, full_path)
    }

    pub fn is_available(&self) -> bool {
        match self {
            Self::Single { root } => Folder::open(root).is_ok(),
            Self::Dual { jpg_root, raw_root } => {
                Folder::open(jpg_root).is_ok() && Folder::open(raw_root).is_ok()
            }
        }
    }
}

/// 校验目录存在且非符号链接/重解析点，并做路径规范化（Windows 去掉 `\\?\` 前缀）。
pub fn normal_root(root: &Path) -> Result<PathBuf> {
    anyhow::ensure!(root.is_dir(), "路径不是有效目录：{}", root.display());
    let _ = Folder::open(root)
        .with_context(|| format!("拒绝打开符号链接或不可访问目录：{}", root.display()))?;
    let absolute = root.canonicalize()?;
    #[cfg(windows)]
    {
        let value = absolute.to_string_lossy();
        if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
            return Ok(PathBuf::from(format!(r"\\{}", rest)));
        }
        if let Some(rest) = value.strip_prefix(r"\\?\") {
            return Ok(PathBuf::from(rest));
        }
    }
    Ok(absolute)
}

fn normalize_workspace_key(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let normalized = trimmed.trim_end_matches(['/', '\\']);
    if normalized.is_empty() {
        Some("/".to_string())
    } else {
        Some(normalized.to_string())
    }
}

fn workspace_key_override() -> Option<String> {
    std::env::var_os("FIRSTCUT_WORKSPACE_KEY")
        .and_then(|v| v.to_str().and_then(normalize_workspace_key))
}

fn canonical_key_str(path: &Path) -> String {
    let raw = path.to_string_lossy();
    #[cfg(windows)]
    {
        raw.replace('/', "\\").trim_end_matches('\\').to_ascii_lowercase()
    }
    #[cfg(not(windows))]
    {
        let trimmed = raw.trim_end_matches('/');
        if trimmed.is_empty() {
            "/".to_string()
        } else {
            trimmed.to_string()
        }
    }
}

/// 本机应用数据根目录（`FIRSTCUT_DATA_DIR` 优先，否则使用系统标准本机应用数据目录）。
pub fn data_root() -> Result<PathBuf> {
    if let Some(val) = std::env::var_os("FIRSTCUT_DATA_DIR") {
        let p = PathBuf::from(val);
        if !p.as_os_str().is_empty() {
            return Ok(std::path::absolute(p)?);
        }
    }
    #[cfg(windows)]
    {
        if let Some(base) = std::env::var_os("LOCALAPPDATA")
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var_os("APPDATA").filter(|s| !s.is_empty()))
        {
            return Ok(PathBuf::from(base).join("firstcut"));
        }
        if let Some(home) = std::env::var_os("USERPROFILE").filter(|s| !s.is_empty()) {
            return Ok(PathBuf::from(home).join(".firstcut-data"));
        }
    }
    #[cfg(not(windows))]
    {
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|s| !s.is_empty()) {
            return Ok(PathBuf::from(xdg).join("firstcut"));
        }
        if let Some(home) = std::env::var_os("HOME").filter(|s| !s.is_empty()) {
            return Ok(PathBuf::from(home).join(".local").join("share").join("firstcut"));
        }
    }
    Ok(std::env::temp_dir().join("firstcut-data"))
}

/// 单个工作区对应的全部本机状态文件路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspacePaths {
    pub state_dir: PathBuf,
    pub cache: PathBuf,
    pub decisions: PathBuf,
    pub report: PathBuf,
    pub scan_report: PathBuf,
    pub config: PathBuf,
    pub ui_preferences: PathBuf,
    pub scene_feedback: PathBuf,
    pub review_log: PathBuf,
    pub thumbs_dir: PathBuf,
    pub meta: PathBuf,
}

impl WorkspacePaths {
    pub fn for_source(source: &WorkspaceSource) -> Result<Self> {
        let dir = data_root()?.join("workspaces").join(source.workspace_id());
        Ok(Self::in_state_dir(dir))
    }

    pub fn in_state_dir(state_dir: PathBuf) -> Self {
        Self {
            cache: state_dir.join("cache.sqlite"),
            decisions: state_dir.join("decisions.sqlite"),
            report: state_dir.join("report.csv"),
            scan_report: state_dir.join("scan-report.csv"),
            config: state_dir.join("config.toml"),
            ui_preferences: state_dir.join("ui-preferences.json"),
            scene_feedback: state_dir.join("scene-feedback.jsonl"),
            review_log: state_dir.join("review.log"),
            thumbs_dir: state_dir.join("thumbs"),
            meta: state_dir.join("workspace.json"),
            state_dir,
        }
    }

    pub fn ensure_state_dir(&self) -> Result<Folder> {
        Folder::open_or_create_explicit(&self.state_dir, true)
            .with_context(|| format!("创建或打开本机工作区目录失败: {}", self.state_dir.display()))
    }
}

/// 兼容旧版：首次打开带 `<相册>/.firstcut/` 的单目录时，只读自动导入旧状态到本机工作区；
/// 绝不修改或删除原 `<相册>/.firstcut/`。
pub fn import_legacy_state_if_needed(
    source: &WorkspaceSource,
    paths: &WorkspacePaths,
) -> Result<()> {
    let WorkspaceSource::Single { root } = source else {
        return Ok(());
    };
    let legacy_folder = match Folder::open(root).and_then(|f| f.state(false)) {
        Ok(dir) => dir,
        Err(_) => return Ok(()),
    };
    let state_folder = paths.ensure_state_dir()?;

    for db_name in ["cache.sqlite", "decisions.sqlite"] {
        if state_folder
            .file(db_name)
            .and_then(|f| f.open_read())
            .ok()
            .flatten()
            .is_some()
        {
            continue;
        }
        let Ok(src_ref) = legacy_folder.file(db_name) else {
            continue;
        };
        if Database::open(src_ref, false).ok().flatten().is_none() {
            continue;
        }
        if let Ok(Some(bytes)) = legacy_folder
            .file(db_name)
            .and_then(|f| f.read(MAX_STATE_IMPORT_BYTES))
        {
            let _ = state_folder.file(db_name)?.write(&bytes);
        }
    }

    for small_name in [
        "burst-overrides.json",
        "config.toml",
        "ui-preferences.json",
        "scene-feedback.jsonl",
    ] {
        if state_folder
            .file(small_name)
            .and_then(|f| f.open_read())
            .ok()
            .flatten()
            .is_some()
        {
            continue;
        }
        if let Ok(Some(bytes)) = legacy_folder
            .file(small_name)
            .and_then(|f| f.read(MAX_SMALL_CONFIG_BYTES))
        {
            let _ = state_folder.file(small_name)?.write(&bytes);
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceMeta {
    pub source: WorkspaceSource,
    pub last_opened_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_export_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentWorkspaceEntry {
    pub id: String,
    pub source: WorkspaceSource,
    pub last_opened_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_export_dir: Option<PathBuf>,
}

pub fn load_workspace_meta(paths: &WorkspacePaths) -> Option<WorkspaceMeta> {
    let dir = Folder::open_or_create_explicit(&paths.state_dir, false).ok()?;
    let bytes = dir.file("workspace.json").ok()?.read(1024 * 1024).ok()??;
    serde_json::from_slice(&bytes).ok()
}

pub fn save_workspace_meta(paths: &WorkspacePaths, meta: &WorkspaceMeta) -> Result<()> {
    let dir = paths.ensure_state_dir()?;
    let bytes = serde_json::to_vec_pretty(meta)?;
    dir.file("workspace.json")?.write(&bytes)?;
    Ok(())
}

pub fn update_last_export_dir(
    paths: &WorkspacePaths,
    source: &WorkspaceSource,
    export_dir: &Path,
) -> Result<()> {
    let now = now_unix_ms();
    let mut meta = load_workspace_meta(paths).unwrap_or_else(|| WorkspaceMeta {
        source: source.clone(),
        last_opened_unix_ms: now,
        last_export_dir: None,
    });
    meta.source = source.clone();
    meta.last_export_dir = Some(export_dir.to_path_buf());
    save_workspace_meta(paths, &meta)?;
    let _ = record_recent_workspace_at(&data_root()?, source, Some(export_dir.to_path_buf()));
    Ok(())
}

pub fn load_recent_workspaces() -> Result<Vec<RecentWorkspaceEntry>> {
    load_recent_workspaces_at(&data_root()?)
}

pub fn load_recent_workspaces_at(base_dir: &Path) -> Result<Vec<RecentWorkspaceEntry>> {
    let dir = match Folder::open_or_create_explicit(base_dir, false) {
        Ok(d) => d,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let Some(bytes) = dir.file("recent-workspaces.json")?.read(2 * 1024 * 1024)? else {
        return Ok(Vec::new());
    };
    let list: Vec<RecentWorkspaceEntry> = serde_json::from_slice(&bytes).unwrap_or_default();
    Ok(list)
}

pub fn record_recent_workspace(
    source: &WorkspaceSource,
    last_export_dir: Option<PathBuf>,
) -> Result<Vec<RecentWorkspaceEntry>> {
    record_recent_workspace_at(&data_root()?, source, last_export_dir)
}

pub fn record_recent_workspace_at(
    base_dir: &Path,
    source: &WorkspaceSource,
    last_export_dir: Option<PathBuf>,
) -> Result<Vec<RecentWorkspaceEntry>> {
    let dir = Folder::open_or_create_explicit(base_dir, true)?;
    let mut list = load_recent_workspaces_at(base_dir).unwrap_or_default();
    let id = source.workspace_id();
    let existing_export = list
        .iter()
        .find(|e| e.id == id)
        .and_then(|e| e.last_export_dir.clone());
    list.retain(|e| e.id != id);
    list.insert(
        0,
        RecentWorkspaceEntry {
            id,
            source: source.clone(),
            last_opened_unix_ms: now_unix_ms(),
            last_export_dir: last_export_dir.or(existing_export),
        },
    );
    list.truncate(MAX_RECENT_WORKSPACES);
    let bytes = serde_json::to_vec_pretty(&list)?;
    dir.file("recent-workspaces.json")?.write(&bytes)?;
    Ok(list)
}

pub fn remove_recent_workspace(id: &str) -> Result<Vec<RecentWorkspaceEntry>> {
    remove_recent_workspace_at(&data_root()?, id)
}

pub fn remove_recent_workspace_at(base_dir: &Path, id: &str) -> Result<Vec<RecentWorkspaceEntry>> {
    let mut list = load_recent_workspaces_at(base_dir)?;
    list.retain(|e| e.id != id);
    let dir = Folder::open_or_create_explicit(base_dir, true)?;
    let bytes = serde_json::to_vec_pretty(&list)?;
    dir.file("recent-workspaces.json")?.write(&bytes)?;
    Ok(list)
}

pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_id_and_dual_validation_behave_consistently() {
        let temp = tempfile::tempdir().unwrap();
        let jpg = temp.path().join("JPG_Card");
        let raw = temp.path().join("RAW_Card");
        let nested = jpg.join("sub");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(&raw).unwrap();

        assert!(WorkspaceSource::dual(&jpg, &jpg).is_err(), "相同目录应拒绝双目录");
        assert!(WorkspaceSource::dual(&jpg, &nested).is_err(), "包含关系目录应拒绝双目录");

        let single = WorkspaceSource::single(&jpg).unwrap();
        let dual = WorkspaceSource::dual(&jpg, &raw).unwrap();
        assert_ne!(single.workspace_id(), dual.workspace_id());
        assert_eq!(dual.workspace_id().len(), 16);
        assert!(dual.is_dual());
    }

    #[test]
    fn legacy_firstcut_state_imports_read_only_without_modifying_album() {
        let temp = tempfile::tempdir().unwrap();
        let album = temp.path().join("album");
        let legacy = album.join(".firstcut");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("config.toml"), b"[weights]\nsharpness = 0.5\n").unwrap();
        std::fs::write(legacy.join("ui-preferences.json"), b"{\"include_raw\":false}").unwrap();

        let source = WorkspaceSource::single(&album).unwrap();
        let paths = WorkspacePaths::in_state_dir(temp.path().join("local_ws"));
        import_legacy_state_if_needed(&source, &paths).unwrap();

        assert_eq!(
            std::fs::read_to_string(&paths.config).unwrap(),
            "[weights]\nsharpness = 0.5\n"
        );
        assert_eq!(
            std::fs::read_to_string(&paths.ui_preferences).unwrap(),
            "{\"include_raw\":false}"
        );

        // 本地已有新版本时不覆盖
        std::fs::write(&paths.config, b"[weights]\nsharpness = 0.9\n").unwrap();
        import_legacy_state_if_needed(&source, &paths).unwrap();
        assert_eq!(
            std::fs::read_to_string(&paths.config).unwrap(),
            "[weights]\nsharpness = 0.9\n"
        );
    }

    #[test]
    fn recent_workspaces_deduplicate_cap_and_remove() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("data");
        for i in 0..12 {
            let dir = temp.path().join(format!("album_{i}"));
            std::fs::create_dir_all(&dir).unwrap();
            let src = WorkspaceSource::single(&dir).unwrap();
            record_recent_workspace_at(&base, &src, None).unwrap();
        }
        let list = load_recent_workspaces_at(&base).unwrap();
        assert_eq!(list.len(), MAX_RECENT_WORKSPACES);

        let first_id = list[0].id.clone();
        let after = remove_recent_workspace_at(&base, &first_id).unwrap();
        assert_eq!(after.len(), MAX_RECENT_WORKSPACES - 1);
        assert!(after.iter().all(|e| e.id != first_id));
    }

    #[test]
    fn workspace_key_override_isolates_same_mount_path() {
        let temp = tempfile::tempdir().unwrap();
        let mount = temp.path().join("photos");
        std::fs::create_dir_all(&mount).unwrap();
        let src = WorkspaceSource::single(&mount).unwrap();

        let base_id = src.workspace_id_with_key(None);
        let id_a = src.workspace_id_with_key(Some("/host/albums/2026-11-concert"));
        let id_a_slash = src.workspace_id_with_key(Some("/host/albums/2026-11-concert/"));
        let id_b = src.workspace_id_with_key(Some("/host/albums/2026-11-portrait"));

        assert_eq!(id_a, id_a_slash, "末尾斜杠不应改变工作区隔离键");
        assert_ne!(id_a, id_b, "相同容器挂载点下不同宿主机相册必须生成不同工作区 ID");
        assert_ne!(id_a, base_id);
        assert_eq!(src.workspace_id_with_key(Some("   ")), base_id);
    }
}
