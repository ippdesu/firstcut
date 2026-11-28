//! 本地 Web 复核与操作台（M-UI1 只读复核 + M-UI2 跑批/星级/配置）
//!
//! axum 服务 + 内嵌 vanilla JS 前端（无 npm 工具链，单 exe 交付）。
//! 写入面（全部明示）：缩略图缓存 `.firstcut/thumbs/`、有界任务日志 `.firstcut/review.log`、
//! XMP 侧车（跑批 --xmp 与 UI 内改星，均沿用他人侧车保护）、配置文件（UI 编辑）、CSV 报告（跑批）。
//! 所有取图请求的路径参数强制限制在扫描根目录内。

pub mod config_edit;
pub mod job;
pub mod scene_feedback;
pub mod snapshot;
pub mod thumb;
mod access;

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::Result;
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{AppendHeaders, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, Semaphore};

use crate::config::ScoreConfig;
use crate::review::job::{JobState, JobStatus};
use crate::storage::{FileRef, Folder};
use crate::workspace::{WorkspacePaths, WorkspaceSource};
use scene_feedback::SceneFeedbackRecord;
use snapshot::Snapshot;

/// 内嵌前端；脚本走独立路由，以便 CSP 禁止内联脚本。
const INDEX_HTML: &[u8] = include_bytes!("assets/index.html");
const REVIEW_JS: &str = include_str!("assets/review.js");

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum UiAction {
    Exit,
    Restart,
    Choose,
    Rawon,
    Rawoff,
}

#[derive(Clone, Serialize)]
struct LoadProgress {
    phase: String,
    done: usize,
    total: usize,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct UiPreferences {
    pub include_raw: bool,
    pub histogram_sample_percent: u8,
}

impl Default for UiPreferences {
    fn default() -> Self { Self { include_raw: true, histogram_sample_percent: 10 } }
}

pub fn load_ui_preferences(root: &Path) -> UiPreferences {
    load_ui_preferences_in(&root.join(".firstcut"))
}

pub fn load_ui_preferences_in(state_dir: &Path) -> UiPreferences {
    match state_file(state_dir, "ui-preferences.json", false).and_then(|file| file.read(16 * 1024)) {
        Ok(Some(bytes)) => serde_json::from_slice(&bytes).unwrap_or_else(|err| {
            quiet_log(format!("[review] 界面偏好无效，使用默认值: {err}"));
            UiPreferences::default()
        }),
        Ok(None) => UiPreferences::default(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => UiPreferences::default(),
        Err(err) => {
            quiet_log(format!("[review] 界面偏好未读取: {err}"));
            UiPreferences::default()
        }
    }
}

/// 默认状态文件统一放在照片根目录，供 CLI、双击启动器和复核服务共用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhotoRootPaths {
    pub state_dir: PathBuf,
    pub cache: PathBuf,
    pub report: PathBuf,
    pub scan_report: PathBuf,
    pub config: PathBuf,
}

pub fn photo_root_paths(root: &Path) -> PhotoRootPaths {
    let state_dir = root.join(".firstcut");
    PhotoRootPaths {
        cache: state_dir.join("cache.sqlite"),
        report: state_dir.join("report.csv"),
        scan_report: state_dir.join("scan-report.csv"),
        config: state_dir.join("config.toml"),
        state_dir,
    }
}

fn save_ui_preferences_in(state_dir: &Path, preferences: &UiPreferences) -> Result<()> {
    state_file(state_dir, "ui-preferences.json", true)?
        .write(&serde_json::to_vec_pretty(preferences)?)?;
    Ok(())
}

fn state_file(state_dir: &Path, name: &str, create: bool) -> std::io::Result<FileRef> {
    Folder::open_or_create_explicit(state_dir, create)?.file(name)
}

pub fn load_review_config(root: &Path, explicit: Option<&Path>) -> Result<(ScoreConfig, bool)> {
    load_review_config_in(&root.join(".firstcut"), explicit)
}

pub fn load_review_config_in(
    state_dir: &Path,
    explicit: Option<&Path>,
) -> Result<(ScoreConfig, bool)> {
    let path = explicit
        .map(Path::to_path_buf)
        .unwrap_or_else(|| state_dir.join("config.toml"));
    let bytes = match FileRef::at(state_dir, &path, false) {
        Ok(file) => file.read(1024 * 1024)?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && explicit.is_none() => None,
        Err(err) => return Err(err.into()),
    };
    match bytes {
        Some(bytes) => Ok((crate::config::load_config_text(
            std::str::from_utf8(&bytes)?, &path.display().to_string())?, true)),
        None if explicit.is_none() => Ok((ScoreConfig::default(), false)),
        None => anyhow::bail!("配置文件不存在：{}", path.display()),
    }
}

/// Windows 的 `canonicalize` 会加上 `\\?\`，与原有缓存中的普通绝对路径不相等。
pub fn normal_photo_root(root: &Path) -> Result<PathBuf> {
    crate::workspace::normal_root(root)
}

struct UiControl {
    action: Mutex<Option<UiAction>>,
    notify: Notify,
    session_id: u64,
}

pub struct AppState {
    source: WorkspaceSource,
    paths: WorkspacePaths,
    root: PathBuf,
    cache_path: PathBuf,
    /// UI 配置编辑的目标文件（--config 指定；缺省本机工作区 `config.toml`）
    config_path: PathBuf,
    config_explicit: bool,
    scene_feedback: RwLock<HashMap<String, SceneFeedbackRecord>>,
    scene_feedback_path: PathBuf,
    image_slots: Arc<Semaphore>,
    thumb_slots: Arc<Semaphore>,
    /// 当前快照；跑批完成后热替换
    snapshot: RwLock<Snapshot>,
    loading: AtomicBool,
    load_error: Mutex<Option<String>>,
    load_progress: Mutex<LoadProgress>,
    include_raw: bool,
    /// 仅本机 Windows 桌面回环模式为 true（允许原生文件夹选择器与打开资源管理器）
    local_desktop: bool,
    /// 本机桌面原生文件夹选择器已授权的外部导出目录（不透明 target_id → 路径，上限 32）
    approved_export_dirs: Mutex<Vec<(String, PathBuf)>>,
    /// 记忆的上次外部导出目录（target_id, display_path）
    last_export_target: Option<(String, String)>,
    /// 全局单后台任务（评分与导出互斥）
    job: Arc<JobState>,
    /// 串行化前端写操作（改星/连拍覆盖/场景反馈/配置保存），防止快速连点触发 SQLite 锁冲突或乱序写 XMP。
    write_lock: Arc<tokio::sync::Mutex<()>>,
    control: Option<Arc<UiControl>>,
}

/// 构建快照并启动服务（阻塞直到服务退出/出错）。
///
/// 绑定成功后（可选）自动打开浏览器。
pub fn serve(
    root: &Path,
    cfg: &ScoreConfig,
    cache_path: &Path,
    port: u16,
    open_browser: bool,
    config_path: Option<&Path>,
) -> Result<()> {
    serve_with_bind(
        root, cfg, cache_path, IpAddr::V4(Ipv4Addr::LOCALHOST), port, open_browser, config_path, None,
    )
}

/// CLI review server with a configurable network bind address.
///
/// The default CLI path remains loopback-only; container users may bind `0.0.0.0`
/// and publish the host port only on loopback.
pub fn serve_with_bind(
    root: &Path,
    cfg: &ScoreConfig,
    cache_path: &Path,
    bind: IpAddr,
    port: u16,
    open_browser: bool,
    config_path: Option<&Path>,
    public_url: Option<&str>,
) -> Result<()> {
    let source = WorkspaceSource::single(root)?;
    let paths = WorkspacePaths::for_source(&source)?;
    crate::workspace::import_legacy_state_if_needed(&source, &paths)?;
    serve_with_bind_workspace(
        &source,
        &paths,
        cfg,
        cache_path,
        bind,
        port,
        open_browser,
        config_path,
        public_url,
    )
}

pub fn serve_with_bind_workspace(
    source: &WorkspaceSource,
    paths: &WorkspacePaths,
    cfg: &ScoreConfig,
    cache_path: &Path,
    bind: IpAddr,
    port: u16,
    open_browser: bool,
    config_path: Option<&Path>,
    public_url: Option<&str>,
) -> Result<()> {
    anyhow::ensure!(port != 0, "复核服务端口必须为 1–65535");
    let configured_key = match std::env::var("FIRSTCUT_ACCESS_TOKEN") {
        Ok(key) => Some(key),
        Err(std::env::VarError::NotPresent) => None,
        Err(err) => return Err(err.into()),
    };
    let access = Arc::new(access::Access::new(bind, port, public_url, configured_key.as_deref())?);
    paths.ensure_state_dir()?;
    let preferences = load_ui_preferences_in(&paths.state_dir);
    let include_raw = if source.is_dual() { true } else { preferences.include_raw };
    serve_once_workspace(
        source,
        paths,
        cfg,
        cache_path,
        bind,
        port,
        open_browser,
        config_path,
        false,
        include_raw,
        true,
        access,
    )?;
    Ok(())
}

/// Windows 双击启动器：直接进入 Web 欢迎页（或在传入初始目录时直接打开），支持单/双目录与无缝切换。
pub fn serve_ui_app(initial_root: Option<&Path>) -> Result<()> {
    let mut active: Option<WorkspaceSource> = initial_root
        .map(WorkspaceSource::single)
        .transpose()?;
    let mut open_browser = true;
    let bind = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let access = Arc::new(access::Access::new(bind, 8787, None, None)?);
    #[cfg(windows)]
    let _reopen_guard = UiReopenListener::start(access.url());
    loop {
        let Some(source) = active.clone() else {
            match serve_welcome(bind, 8787, open_browser, access.clone())? {
                WelcomeOutcome::Open(next) => {
                    active = Some(next);
                    open_browser = false;
                }
                WelcomeOutcome::Exit => return Ok(()),
            }
            continue;
        };

        let paths = WorkspacePaths::for_source(&source)?;
        crate::workspace::import_legacy_state_if_needed(&source, &paths)?;
        paths.ensure_state_dir()?;
        let _ = crate::workspace::record_recent_workspace(&source, None);
        let (config, _) = load_review_config_in(&paths.state_dir, None)?;
        let preferences = load_ui_preferences_in(&paths.state_dir);
        let include_raw = if source.is_dual() { true } else { preferences.include_raw };
        match serve_once_workspace(
            &source,
            &paths,
            &config,
            &paths.cache,
            bind,
            8787,
            open_browser,
            None,
            true,
            include_raw,
            true,
            access.clone(),
        )? {
            Some(UiAction::Restart) => open_browser = false,
            Some(UiAction::Choose) => {
                active = None;
                open_browser = false;
            }
            Some(UiAction::Rawon) => {
                let mut preferences = load_ui_preferences_in(&paths.state_dir);
                preferences.include_raw = true;
                save_ui_preferences_in(&paths.state_dir, &preferences)?;
                open_browser = false;
            }
            Some(UiAction::Rawoff) => {
                let mut preferences = load_ui_preferences_in(&paths.state_dir);
                preferences.include_raw = false;
                save_ui_preferences_in(&paths.state_dir, &preferences)?;
                open_browser = false;
            }
            _ => return Ok(()),
        }
    }
}

/// 兼容旧签名：保留供外部调用方使用。
pub fn serve_ui(root: &Path, _pick_root: impl Fn() -> Option<PathBuf>) -> Result<()> {
    serve_ui_app(Some(root))
}

enum WelcomeOutcome {
    Open(WorkspaceSource),
    Exit,
}

struct WelcomeState {
    local_desktop: bool,
    approved_picks: Mutex<Vec<(String, PathBuf)>>,
    outcome: Mutex<Option<WelcomeOutcome>>,
    notify: Notify,
    session_id: u64,
}

fn serve_welcome(
    bind: IpAddr,
    port: u16,
    open_browser: bool,
    access: Arc<access::Access>,
) -> Result<WelcomeOutcome> {
    let state = Arc::new(WelcomeState {
        local_desktop: cfg!(windows) && bind.is_loopback(),
        approved_picks: Mutex::new(Vec::new()),
        outcome: Mutex::new(None),
        notify: Notify::new(),
        session_id: NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed),
    });
    let shutdown_state = Arc::clone(&state);
    let router_state = Arc::clone(&state);
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(async move {
        let app = welcome_router(router_state, access.clone());
        let listener = tokio::net::TcpListener::bind((bind, port)).await?;
        let url = access.url();
        if open_browser {
            if let Err(err) = open_review_browser(&url) {
                quiet_log(format!("[review] 无法自动打开浏览器：{err:#}"));
            }
        }
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                shutdown_state.notify.notified().await;
            })
            .await
    })?;
    let outcome = state
        .outcome
        .lock()
        .unwrap()
        .take()
        .unwrap_or(WelcomeOutcome::Exit);
    Ok(outcome)
}

fn welcome_router(state: Arc<WelcomeState>, access: Arc<access::Access>) -> Router {
    let session = Router::new()
        .route("/api/session", post(access::sign_in))
        .with_state(access.clone());
    Router::new()
        .route("/", get(index))
        .route("/review.js", get(review_js))
        .route("/healthz", get(|| async { access::HEALTH_MARKER }))
        .route("/api/workspace/status", get(welcome_status))
        .route("/api/workspace/pick-folder", post(welcome_pick_folder))
        .route("/api/workspace/open", post(welcome_open))
        .route("/api/workspace/recent/remove", post(welcome_recent_remove))
        .route("/api/control", get(welcome_control_status).post(welcome_control_post))
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024))
        .with_state(state)
        .merge(session.layer(axum::extract::DefaultBodyLimit::max(1024)))
        .layer(axum::middleware::from_fn_with_state(access, access::guard))
}

#[cfg(any(windows, test))]
fn mint_pick_token(approved: &Mutex<Vec<(String, PathBuf)>>, path: PathBuf) -> Result<String> {
    let mut entropy = [0u8; 16];
    getrandom::fill(&mut entropy).map_err(|err| anyhow::anyhow!("{err}"))?;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    let token = URL_SAFE_NO_PAD.encode(entropy);
    let mut list = approved.lock().unwrap();
    if list.len() >= APPROVED_EXPORT_CAP {
        list.remove(0);
    }
    list.push((token.clone(), path));
    Ok(token)
}

fn resolve_pick_token(approved: &Mutex<Vec<(String, PathBuf)>>, token: &str) -> Option<PathBuf> {
    let list = approved.lock().unwrap();
    list.iter()
        .find(|(id, _)| id == token)
        .map(|(_, path)| path.clone())
}

fn recent_json_list() -> Vec<serde_json::Value> {
    let list = crate::workspace::load_recent_workspaces().unwrap_or_default();
    list.into_iter()
        .map(|entry| {
            let available = entry.source.is_available();
            let (mode, jpg_root, raw_root) = match &entry.source {
                WorkspaceSource::Single { root } => ("single", root.display().to_string(), None),
                WorkspaceSource::Dual { jpg_root, raw_root } => (
                    "dual",
                    jpg_root.display().to_string(),
                    Some(raw_root.display().to_string()),
                ),
            };
            serde_json::json!({
                "id": entry.id,
                "mode": mode,
                "display_label": entry.source.display_label(),
                "jpg_root": jpg_root,
                "raw_root": raw_root,
                "available": available,
                "last_opened_unix_ms": entry.last_opened_unix_ms,
                "last_export_dir": entry.last_export_dir.map(|p| p.display().to_string()),
            })
        })
        .collect()
}

async fn welcome_status(State(state): State<Arc<WelcomeState>>) -> Response {
    json_response(serde_json::json!({
        "state": "welcome",
        "local_desktop": state.local_desktop,
        "recent": recent_json_list(),
    }))
}

#[derive(Deserialize)]
struct WelcomePickRequest {
    #[serde(default)]
    role: String,
}

async fn welcome_pick_folder(
    State(state): State<Arc<WelcomeState>>,
    axum::Json(req): axum::Json<WelcomePickRequest>,
) -> Response {
    if !state.local_desktop {
        return (StatusCode::NOT_FOUND, "当前运行环境不支持系统文件夹选择器").into_response();
    }
    #[cfg(windows)]
    {
        let title = match req.role.as_str() {
            "jpg" => "选择 JPG 照片文件夹（例如相机卡 1）",
            "raw" => "选择 RAW 原片文件夹（例如相机卡 2）",
            _ => "选择照片文件夹（支持子目录及 JPG+RAW 同目录）",
        };
        let picked = match tokio::task::spawn_blocking(move || {
            rfd::FileDialog::new().set_title(title).pick_folder()
        })
        .await
        {
            Ok(p) => p,
            Err(err) => return internal_error(err.into()),
        };
        let Some(path) = picked else {
            return json_response(serde_json::json!({ "cancelled": true }));
        };
        let normalized = match normal_photo_root(&path) {
            Ok(p) => p,
            Err(err) => return (StatusCode::BAD_REQUEST, format!("{err:#}")).into_response(),
        };
        let token = match mint_pick_token(&state.approved_picks, normalized.clone()) {
            Ok(t) => t,
            Err(err) => return internal_error(err),
        };
        json_response(serde_json::json!({
            "cancelled": false,
            "pick_id": token,
            "display_path": normalized.display().to_string(),
        }))
    }
    #[cfg(not(windows))]
    {
        let _ = &req.role;
        (StatusCode::NOT_FOUND, "当前平台不支持系统文件夹选择器").into_response()
    }
}

fn apply_welcome_single_preferences(
    source: &WorkspaceSource,
    paths: &WorkspacePaths,
    include_raw: bool,
) {
    let _ = crate::workspace::import_legacy_state_if_needed(source, paths);
    let mut prefs = load_ui_preferences_in(&paths.state_dir);
    prefs.include_raw = include_raw;
    let _ = save_ui_preferences_in(&paths.state_dir, &prefs);
}

#[derive(Deserialize)]
struct WelcomeOpenRequest {
    #[serde(default)]
    recent_id: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    root_pick_id: Option<String>,
    #[serde(default)]
    jpg_pick_id: Option<String>,
    #[serde(default)]
    raw_pick_id: Option<String>,
    #[serde(default)]
    include_raw: Option<bool>,
}

async fn welcome_open(
    State(state): State<Arc<WelcomeState>>,
    axum::Json(req): axum::Json<WelcomeOpenRequest>,
) -> Response {
    if !state.local_desktop {
        return (StatusCode::FORBIDDEN, "非本机桌面模式不允许通过网页切换目录").into_response();
    }
    let source = if let Some(recent_id) = req.recent_id.as_deref().filter(|s| !s.is_empty()) {
        let list = crate::workspace::load_recent_workspaces().unwrap_or_default();
        let Some(found) = list.into_iter().find(|e| e.id == recent_id) else {
            return (StatusCode::NOT_FOUND, "该最近工作区记录不存在").into_response();
        };
        let verified = match &found.source {
            WorkspaceSource::Single { root } => WorkspaceSource::single(root),
            WorkspaceSource::Dual { jpg_root, raw_root } => WorkspaceSource::dual(jpg_root, raw_root),
        };
        match verified {
            Ok(s) => s,
            Err(err) => {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("工作区目录当前不可访问（请确认存储卡或磁盘已连接）: {err:#}"),
                )
                    .into_response();
            }
        }
    } else {
        match req.mode.as_deref() {
            Some("single") => {
                let Some(token) = req.root_pick_id.as_deref().filter(|s| !s.is_empty()) else {
                    return (StatusCode::BAD_REQUEST, "请先选择照片文件夹").into_response();
                };
                let Some(path) = resolve_pick_token(&state.approved_picks, token) else {
                    return (StatusCode::BAD_REQUEST, "目录授权已过期，请重新选择文件夹").into_response();
                };
                let s = match WorkspaceSource::single(&path) {
                    Ok(s) => s,
                    Err(err) => return (StatusCode::BAD_REQUEST, format!("{err:#}")).into_response(),
                };
                if let Some(inc_raw) = req.include_raw {
                    if let Ok(paths) = WorkspacePaths::for_source(&s) {
                        apply_welcome_single_preferences(&s, &paths, inc_raw);
                    }
                }
                s
            }
            Some("dual") => {
                let Some(jpg_token) = req.jpg_pick_id.as_deref().filter(|s| !s.is_empty()) else {
                    return (StatusCode::BAD_REQUEST, "请先选择 JPG 文件夹").into_response();
                };
                let Some(raw_token) = req.raw_pick_id.as_deref().filter(|s| !s.is_empty()) else {
                    return (StatusCode::BAD_REQUEST, "请先选择 RAW 文件夹").into_response();
                };
                let Some(jpg_path) = resolve_pick_token(&state.approved_picks, jpg_token) else {
                    return (StatusCode::BAD_REQUEST, "JPG 目录授权已过期，请重新选择").into_response();
                };
                let Some(raw_path) = resolve_pick_token(&state.approved_picks, raw_token) else {
                    return (StatusCode::BAD_REQUEST, "RAW 目录授权已过期，请重新选择").into_response();
                };
                match WorkspaceSource::dual(&jpg_path, &raw_path) {
                    Ok(s) => s,
                    Err(err) => return (StatusCode::BAD_REQUEST, format!("{err:#}")).into_response(),
                }
            }
            _ => return (StatusCode::BAD_REQUEST, "无效的工作区模式").into_response(),
        }
    };

    *state.outcome.lock().unwrap() = Some(WelcomeOutcome::Open(source));
    state.notify.notify_one();
    json_response(serde_json::json!({ "ok": true }))
}

#[derive(Deserialize)]
struct RecentRemoveRequest {
    id: String,
}

async fn welcome_recent_remove(
    State(state): State<Arc<WelcomeState>>,
    axum::Json(req): axum::Json<RecentRemoveRequest>,
) -> Response {
    if !state.local_desktop {
        return (StatusCode::FORBIDDEN, "非本机桌面模式").into_response();
    }
    if let Err(err) = crate::workspace::remove_recent_workspace(&req.id) {
        return internal_error(err);
    }
    json_response(serde_json::json!({
        "ok": true,
        "recent": recent_json_list(),
    }))
}

async fn welcome_control_status(State(state): State<Arc<WelcomeState>>) -> Response {
    json_response(serde_json::json!({
        "enabled": true,
        "session": state.session_id,
        "include_raw": true,
        "local_desktop": state.local_desktop,
    }))
}

async fn welcome_control_post(
    State(state): State<Arc<WelcomeState>>,
    axum::Json(request): axum::Json<ControlRequest>,
) -> Response {
    if request.action == UiAction::Exit {
        *state.outcome.lock().unwrap() = Some(WelcomeOutcome::Exit);
        state.notify.notify_one();
        return json_response(serde_json::json!({ "ok": true }));
    }
    (StatusCode::BAD_REQUEST, "当前位于入口页，不支持此控制操作").into_response()
}

fn serve_once_workspace(
    source: &WorkspaceSource,
    paths: &WorkspacePaths,
    cfg: &ScoreConfig,
    cache_path: &Path,
    bind: IpAddr,
    port: u16,
    open_browser: bool,
    config_path: Option<&Path>,
    ui_control: bool,
    include_raw: bool,
    skip_processed: bool,
    access: Arc<access::Access>,
) -> Result<Option<UiAction>> {
    let root = source.jpg_root();
    let scene_feedback_path = paths.scene_feedback.clone();
    let feedback = scene_feedback::load_latest(&paths.state_dir, &scene_feedback_path)?;
    let initial_snapshot = if ui_control {
        Snapshot::empty_for_source(source, cfg)
    } else {
        build_review_snapshot_for_workspace(
            source,
            paths,
            cfg,
            cache_path,
            &feedback,
            include_raw,
            skip_processed,
            &|_, _, _| {},
        )?
    };
    let load_feedback = feedback.clone();

    let config_explicit = config_path.is_some();
    let config_path = config_path
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| paths.config.clone());
    let control = ui_control.then(|| Arc::new(UiControl {
        action: Mutex::new(None),
        notify: Notify::new(),
        session_id: NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed),
    }));
    let local_desktop = cfg!(windows) && bind.is_loopback();
    let mut approved_init: Vec<(String, PathBuf)> = Vec::new();
    let last_export_target = if local_desktop {
        crate::workspace::load_workspace_meta(paths)
            .and_then(|m| m.last_export_dir)
            .filter(|dir| dir.is_dir())
            .and_then(|dir| {
                let mut entropy = [0u8; 16];
                getrandom::fill(&mut entropy).ok()?;
                use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
                let token = URL_SAFE_NO_PAD.encode(entropy);
                let display = dir.display().to_string();
                approved_init.push((token.clone(), dir));
                Some((token, display))
            })
    } else {
        None
    };

    let state = Arc::new(AppState {
        source: source.clone(),
        paths: paths.clone(),
        root: root.to_path_buf(),
        cache_path: cache_path.to_path_buf(),
        config_path,
        config_explicit,
        scene_feedback: RwLock::new(feedback),
        scene_feedback_path,
        image_slots: Arc::new(Semaphore::new(16)),
        thumb_slots: Arc::new(Semaphore::new(3)),
        snapshot: RwLock::new(initial_snapshot),
        loading: AtomicBool::new(ui_control),
        load_error: Mutex::new(None),
        load_progress: Mutex::new(LoadProgress {
            phase: "准备读取照片".into(), done: 0, total: 0,
        }),
        include_raw,
        local_desktop,
        approved_export_dirs: Mutex::new(approved_init),
        last_export_target,
        job: Arc::new(JobState::with_log_file(&paths.state_dir, paths.review_log.clone())),
        write_lock: Arc::new(tokio::sync::Mutex::new(())),
        control: control.clone(),
    });
    let load_state = Arc::clone(&state);
    let load_source = source.clone();
    let load_paths = paths.clone();
    let load_cfg = cfg.clone();
    let load_cache = cache_path.to_path_buf();
    let shutdown_control = control.clone();
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(async move {
        let app = router(state, access.clone());
        let listener = tokio::net::TcpListener::bind((bind, port)).await?;
        if ui_control {
            std::thread::spawn(move || {
                let on_progress = |phase: &str, done: usize, total: usize| {
                    *load_state.load_progress.lock().unwrap() = LoadProgress {
                        phase: phase.into(), done, total,
                    };
                };
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    build_review_snapshot_for_workspace(
                        &load_source,
                        &load_paths,
                        &load_cfg,
                        &load_cache,
                        &load_feedback,
                        include_raw,
                        skip_processed,
                        &on_progress,
                    )
                }));
                match result {
                    Ok(Ok(snapshot)) => replace_snapshot(&load_state.snapshot, snapshot),
                    Ok(Err(err)) => *load_state.load_error.lock().unwrap() = Some(format!("{err:#}")),
                    Err(_) => *load_state.load_error.lock().unwrap() =
                        Some("读取照片时发生内部错误；请重新选择目录或重启服务。".into()),
                }
                load_state.loading.store(false, Ordering::Release);
            });
        }
        let url = access.url();
        if open_browser {
            if let Err(err) = open_review_browser(&url) {
                quiet_log(format!("[review] 无法自动打开浏览器：{err:#}"));
            }
        }
        if !ui_control {
            if access.is_managed() {
                quiet_log(format!("[review] 就绪: {}（使用已配置的访问凭据登录）", access.origin()));
            } else {
                quiet_log(format!("[review] 复核链接: {url}（仅供本人使用，Ctrl+C 退出）"));
            }
        }
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                if let Some(control) = shutdown_control {
                    control.notify.notified().await;
                } else {
                    std::future::pending::<()>().await;
                }
            })
            .await
    })?;
    Ok(control.and_then(|control| control.action.lock().unwrap().take()))
}

fn router(state: Arc<AppState>, access: Arc<access::Access>) -> Router {
    let session = Router::new()
        .route("/api/session", post(access::sign_in))
        .with_state(access.clone());
    let batch_flag = Router::new()
        .route("/api/flag/batch", post(set_flag_batch))
        .layer(axum::extract::DefaultBodyLimit::max(2 * 1024 * 1024))
        .with_state(Arc::clone(&state));
    Router::new()
        .route("/", get(index))
        .route("/review.js", get(review_js))
        .route("/healthz", get(|| async { access::HEALTH_MARKER }))
        .route("/api/workspace/status", get(workspace_status))
        .route("/api/photos", get(photos))
        .route("/api/load", get(load_status))
        .route("/thumb", get(thumb))
        .route("/image", get(image))
        .route("/api/job", get(job_status))
        .route("/api/score/run", post(score_run))
        .route("/api/export/pick-folder", post(export_pick_folder))
        .route("/api/export/preview", post(export_preview))
        .route("/api/export/run", post(export_run))
        .route("/api/export/cancel", post(export_cancel))
        .route("/api/export/open", post(export_open))
        .route("/api/rate", post(rate))
        .route("/api/flag", post(set_flag))
        .route("/api/burst/keep", post(burst_keep))
        .route("/api/scene-feedback", post(scene_feedback_save))
        .route("/api/config", get(config_get).post(config_save))
        .route("/api/ui-settings", get(ui_settings_get).post(ui_settings_save))
        .route("/api/control", get(control_status).post(control_post))
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024))
        .with_state(state)
        .merge(batch_flag)
        .merge(session.layer(axum::extract::DefaultBodyLimit::max(1024)))
        .layer(axum::middleware::from_fn_with_state(access, access::guard))
}

pub fn open_review_browser(url: &str) -> Result<()> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};
        let wide: Vec<u16> = url.encode_utf16().chain([0]).collect();
        let result = unsafe { ShellExecuteW(std::ptr::null_mut(), std::ptr::null(),
            wide.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL) };
        anyhow::ensure!(result as isize > 32, "系统无法打开默认浏览器（错误 {}）", result as isize);
    }
    #[cfg(not(windows))]
    {
        let program = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        std::process::Command::new(program).arg(url).spawn()?;
    }
    Ok(())
}

/// Windows 单实例唤起信号：再次双击 `firstcut-ui.exe` 时通知已在运行的进程重新打开带
/// `#access=` 的复核链接，避免在浏览器清缓存或更换默认浏览器后被锁在登录页，且不落盘凭据。
#[cfg(windows)]
const UI_REOPEN_EVENT: &str = "Local\\firstcut-ui-reopen-8787";

#[cfg(windows)]
#[link(name = "Kernel32")]
extern "system" {
    fn CreateEventW(
        lp_event_attributes: *mut std::ffi::c_void,
        b_manual_reset: i32,
        b_initial_state: i32,
        lp_name: *const u16,
    ) -> isize;
    fn OpenEventW(dw_desired_access: u32, b_inherit_handle: i32, lp_name: *const u16) -> isize;
    fn SetEvent(h_event: isize) -> i32;
    fn WaitForSingleObject(h_handle: isize, dw_milliseconds: u32) -> u32;
    fn CloseHandle(h_object: isize) -> i32;
}

#[cfg(windows)]
pub fn signal_ui_reopen() -> bool {
    const EVENT_MODIFY_STATE: u32 = 0x0002;
    let wide: Vec<u16> = UI_REOPEN_EVENT.encode_utf16().chain([0]).collect();
    let handle = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, wide.as_ptr()) };
    if handle == 0 {
        return false;
    }
    let ok = unsafe { SetEvent(handle) } != 0;
    unsafe { CloseHandle(handle) };
    ok
}

#[cfg(windows)]
struct UiReopenListener {
    handle: isize,
    stopped: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(windows)]
impl UiReopenListener {
    fn start(url: String) -> Option<Self> {
        const INFINITE: u32 = 0xFFFF_FFFF;
        const WAIT_OBJECT_0: u32 = 0;
        let wide: Vec<u16> = UI_REOPEN_EVENT.encode_utf16().chain([0]).collect();
        let handle = unsafe { CreateEventW(std::ptr::null_mut(), 0, 0, wide.as_ptr()) };
        if handle == 0 {
            return None;
        }
        let stopped = Arc::new(AtomicBool::new(false));
        let thread_stopped = Arc::clone(&stopped);
        let thread = std::thread::spawn(move || {
            while unsafe { WaitForSingleObject(handle, INFINITE) } == WAIT_OBJECT_0 {
                if thread_stopped.load(Ordering::Acquire) {
                    break;
                }
                let _ = open_review_browser(&url);
            }
        });
        Some(Self {
            handle,
            stopped,
            thread: Some(thread),
        })
    }
}

#[cfg(windows)]
impl Drop for UiReopenListener {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        unsafe { SetEvent(self.handle) };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        unsafe { CloseHandle(self.handle) };
    }
}

fn build_review_snapshot_for_workspace(
    source: &WorkspaceSource,
    paths: &WorkspacePaths,
    cfg: &ScoreConfig,
    cache_path: &Path,
    feedback: &HashMap<String, SceneFeedbackRecord>,
    include_raw: bool,
    skip_processed: bool,
    progress: &dyn Fn(&str, usize, usize),
) -> Result<Snapshot> {
    quiet_log("[review] 正在构建快照（扫描 + 缓存）……");
    let snapshot = snapshot::build_snapshot_for_workspace(
        source,
        paths,
        cfg,
        cache_path,
        feedback,
        include_raw,
        skip_processed,
        progress,
    )?;
    let scored = snapshot.photos.iter().filter(|p| p.scores.is_some()).count();
    quiet_log(format!("[review] 快照就绪：{} 张照片（已评分 {}，未评分 {}）",
        snapshot.photos.len(), scored, snapshot.photos.len() - scored));
    Ok(snapshot)
}

async fn workspace_status(State(state): State<Arc<AppState>>) -> Response {
    let raw_root = state
        .source
        .is_dual()
        .then(|| state.source.raw_root().display().to_string());
    let (last_export_target_id, last_export_display) = match &state.last_export_target {
        Some((id, disp)) => (Some(id.as_str()), Some(disp.as_str())),
        None => (None, None),
    };
    json_response(serde_json::json!({
        "state": "active",
        "workspace_id": state.source.workspace_id(),
        "source_mode": state.source.mode_str(),
        "jpg_root": state.source.jpg_root().display().to_string(),
        "raw_root": raw_root,
        "display_label": state.source.display_label(),
        "include_raw": state.include_raw,
        "local_desktop": state.local_desktop,
        "last_export_target_id": last_export_target_id,
        "last_export_display": last_export_display,
    }))
}

fn quiet_log(message: impl AsRef<str>) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "{}", message.as_ref());
}

async fn index() -> Response {
    (
        AppendHeaders([
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ]),
        INDEX_HTML,
    )
        .into_response()
}

async fn review_js() -> Response {
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], REVIEW_JS).into_response()
}

async fn photos(State(state): State<Arc<AppState>>) -> Response {
    if state.loading.load(Ordering::Acquire) {
        return (StatusCode::SERVICE_UNAVAILABLE, "正在读取照片和评分").into_response();
    }
    if let Some(error) = state.load_error.lock().unwrap().as_ref() {
        return (StatusCode::INTERNAL_SERVER_ERROR, error.clone()).into_response();
    }
    let guard = state.snapshot.read().unwrap();
    match serde_json::to_vec(&*guard) {
        Ok(bytes) => (
            AppendHeaders([(header::CONTENT_TYPE, "application/json")]),
            bytes,
        )
            .into_response(),
        Err(err) => internal_error(err.into()),
    }
}

#[derive(Deserialize)]
struct PhotoParam {
    p: String,
}

/// 缩略图：磁盘缓存命中直出，否则按需生成
async fn thumb(State(state): State<Arc<AppState>>, Query(q): Query<PhotoParam>) -> Response {
    let Ok(slot) = state.thumb_slots.clone().acquire_owned().await else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match tokio::task::spawn_blocking(move || {
        let _slot = slot;
        let file = resolve_jpeg(&state, &q.p)?;
        thumb::get_or_create(&state.paths.state_dir, &q.p, file).map_err(internal_error)
    }).await {
        Ok(Ok(Some(bytes))) => ([(header::CONTENT_TYPE, "image/jpeg")], bytes).into_response(),
        Ok(Ok(None)) => StatusCode::NOT_FOUND.into_response(),
        Ok(Err(response)) => response,
        Err(err) => internal_error(err.into()),
    }
}

/// 原图直读（浏览器原生解码，1:1 预览）
async fn image(State(state): State<Arc<AppState>>, Query(q): Query<PhotoParam>) -> Response {
    let Ok(slot) = state.image_slots.clone().try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let (file, size) = match tokio::task::spawn_blocking(move || {
        let mut file = resolve_jpeg(&state, &q.p)?;
        let size = file.metadata().map_err(|err| internal_error(err.into()))?.len();
        if size > crate::decode::MAX_PHOTO_BYTES {
            return Err(StatusCode::PAYLOAD_TOO_LARGE.into_response());
        }
        if !crate::decode::can_preview_jpeg(&mut file).map_err(|err| internal_error(err.into()))? {
            return Err((StatusCode::UNPROCESSABLE_ENTITY, "JPEG 文件无法预览，或尺寸超出上限").into_response());
        }
        Ok((file, size))
    }).await {
        Ok(Ok(photo)) => photo,
        Ok(Err(response)) => return response,
        Err(err) => return internal_error(err.into()),
    };
    use tokio::io::AsyncReadExt;
    let reader = tokio::fs::File::from_std(file).take(size);
    use futures_util::Stream;
    let mut stream = Box::pin(tokio_util::io::ReaderStream::new(reader));
    let stream = futures_util::stream::poll_fn(move |cx| {
        let _slot = &slot; // Hold through the whole transfer, including a slow client.
        stream.as_mut().poll_next(cx)
    });
    (
        AppendHeaders([(header::CONTENT_TYPE, "image/jpeg"), (header::CONTENT_LENGTH, &size.to_string())]),
        axum::body::Body::from_stream(stream),
    ).into_response()
}

// ---- M-UI2：跑批 / 星级 / 配置 ----

/// GET /api/job：当前任务状态 + 最近日志（前端 600ms 轮询；兼容评分与导出）
async fn job_status(State(state): State<Arc<AppState>>) -> Response {
    let snap = state.job.detail_snapshot();
    let kind = snap.kind.as_str();
    let can_open_export = snap.can_open_export && state.local_desktop;
    let body = match snap.status {
        JobStatus::Idle => serde_json::json!({
            "state": "idle",
            "kind": kind,
            "log": snap.log,
            "cancellable": false,
            "can_open_export": can_open_export,
        }),
        JobStatus::Running { done, total } => serde_json::json!({
            "state": "running",
            "kind": kind,
            "done": done,
            "total": total,
            "bytes_done": snap.bytes_done,
            "bytes_total": snap.bytes_total,
            "current_file": snap.current_file,
            "cancellable": snap.cancellable,
            "can_open_export": false,
            "log": snap.log,
        }),
        JobStatus::Done { summary } => serde_json::json!({
            "state": "done",
            "kind": kind,
            "summary": summary,
            "bytes_done": snap.bytes_done,
            "bytes_total": snap.bytes_total,
            "cancellable": false,
            "can_open_export": can_open_export,
            "log": snap.log,
        }),
        JobStatus::Failed { error } => serde_json::json!({
            "state": "failed",
            "kind": kind,
            "error": error,
            "cancellable": false,
            "can_open_export": can_open_export,
            "log": snap.log,
        }),
    };
    json_response(body)
}

async fn load_status(State(state): State<Arc<AppState>>) -> Response {
    let progress = state.load_progress.lock().unwrap().clone();
    let error = state.load_error.lock().unwrap().clone();
    json_response(serde_json::json!({
        "loading": state.loading.load(Ordering::Acquire),
        "phase": progress.phase, "done": progress.done, "total": progress.total,
        "error": error,
    }))
}

async fn control_status(State(state): State<Arc<AppState>>) -> Response {
    json_response(match &state.control {
        Some(control) => serde_json::json!({
            "enabled": true,
            "session": control.session_id,
            "include_raw": state.include_raw,
            "local_desktop": state.local_desktop,
        }),
        None => serde_json::json!({
            "enabled": false,
            "include_raw": state.include_raw,
            "local_desktop": state.local_desktop,
        }),
    })
}

#[derive(Deserialize)]
struct UiSettingsRequest {
    histogram_sample_percent: u8,
}

async fn ui_settings_get(State(state): State<Arc<AppState>>) -> Response {
    let state_dir = state.paths.state_dir.clone();
    match tokio::task::spawn_blocking(move || load_ui_preferences_in(&state_dir)).await {
        Ok(preferences) => json_response(serde_json::json!({
            "histogram_sample_percent": preferences.histogram_sample_percent,
        })),
        Err(err) => internal_error(err.into()),
    }
}

/// 串行化且对请求取消安全的写操作执行器：
/// 先获取全局写锁与任务守卫，再将二者与 `AppState` 一并移入 `spawn_blocking`；
/// 即使 HTTP 连接在等待落盘期间断开导致外层 handler future 被 drop，
/// 后台阻塞任务仍会持锁并持有守卫直到落盘、内存状态与快照全部更新完毕。
async fn run_serialized_mutation<F>(
    state: Arc<AppState>,
    busy_msg: &'static str,
    work: F,
) -> Response
where
    F: FnOnce(&AppState) -> Response + Send + 'static,
{
    let write_guard = Arc::clone(&state.write_lock).lock_owned().await;
    let Some(mutation) = state.job.begin_mutation() else {
        return (StatusCode::CONFLICT, busy_msg).into_response();
    };
    let state_for_task = Arc::clone(&state);
    match tokio::task::spawn_blocking(move || {
        let _write_guard = write_guard;
        let _mutation = mutation;
        work(&state_for_task)
    })
    .await
    {
        Ok(response) => response,
        Err(err) => internal_error(err.into()),
    }
}

async fn ui_settings_save(
    State(state): State<Arc<AppState>>,
    axum::Json(request): axum::Json<UiSettingsRequest>,
) -> Response {
    if !(1..=25).contains(&request.histogram_sample_percent) {
        return (StatusCode::BAD_REQUEST, "直方图抽样比例必须在 1% 到 25% 之间").into_response();
    }
    let sample_percent = request.histogram_sample_percent;
    run_serialized_mutation(
        state,
        "评分任务进行中或服务正在关闭，暂不能保存设置",
        move |state| {
            let mut preferences = load_ui_preferences_in(&state.paths.state_dir);
            preferences.histogram_sample_percent = sample_percent;
            match save_ui_preferences_in(&state.paths.state_dir, &preferences) {
                Ok(()) => json_response(serde_json::json!({
                    "ok": true,
                    "histogram_sample_percent": preferences.histogram_sample_percent,
                })),
                Err(error) => internal_error(error),
            }
        },
    )
    .await
}

#[derive(Deserialize)]
struct ControlRequest {
    action: UiAction,
}

async fn control_post(
    State(state): State<Arc<AppState>>,
    axum::Json(request): axum::Json<ControlRequest>,
) -> Response {
    let Some(control) = &state.control else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !state.job.begin_shutdown() {
        return (StatusCode::CONFLICT, "评分正在运行或服务已在关闭，请稍后再试").into_response();
    }
    *control.action.lock().unwrap() = Some(request.action);
    control.notify.notify_one();
    json_response(serde_json::json!({ "ok": true }))
}

#[derive(Deserialize)]
struct ScoreRunParams {
    /// 跑批后写 XMP 星级侧车
    #[serde(default)]
    xmp: bool,
    /// 显式将建议曝光写入 Adobe `crs:Exposure2012`（需同时开启 `xmp`；不覆盖已有曝光）
    #[serde(default)]
    xmp_exposure: bool,
    /// 跳过 AI 推理（纯像素快速预览）
    #[serde(default)]
    no_ai: bool,
}

/// POST /api/score/run：触发一次完整评分（单任务互斥；完成后热替换快照）
async fn score_run(State(state): State<Arc<AppState>>,
    axum::Json(params): axum::Json<ScoreRunParams>) -> Response {
    if params.xmp_exposure && !params.xmp {
        return (StatusCode::BAD_REQUEST, "写入 Adobe 建议曝光需要同时开启 XMP 侧车").into_response();
    }
    if state.loading.load(Ordering::Acquire) {
        return (StatusCode::CONFLICT, "照片仍在加载，请稍后再评分").into_response();
    }
    if !state.job.begin() {
        return (
            StatusCode::CONFLICT,
            "已有评分任务在运行，请等待完成",
        )
            .into_response();
    }

    let st = Arc::clone(&state);
    let job = Arc::clone(&state.job);
    let source = state.source.clone();
    let paths = state.paths.clone();
    let cache_path = state.cache_path.clone();
    let config_path = state.config_path.clone();
    let no_ai = params.no_ai;
    let _ = (params.xmp, params.xmp_exposure);
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        job.log("[score] 任务开始");
        // 每次跑批都重新加载配置：UI 里的配置编辑保存后下一次跑批即生效
        let (cfg, config_exists) = match load_review_config_in(&paths.state_dir,
            st.config_explicit.then_some(config_path.as_path())) {
            Ok(result) => result,
            Err(err) => {
                job.log(format!(
                    "[score] 配置加载失败（耗时 {:.1} 秒）: {err:#}",
                    started.elapsed().as_secs_f64()
                ));
                job.fail(format!("配置加载失败: {err:#}"));
                return;
            }
        };
        job.log(if config_exists {
            format!("[score] 配置已加载: {}", config_path.display())
        } else {
            "[score] 使用内置默认配置（工作区尚无 UI 配置）".into()
        });

        let opts = crate::score::ScoreJobOptions {
            output_csv: paths.report.clone(),
            raw_csv: false,
            cache_path: cache_path.clone(),
            no_cache: false,
            no_ai,
            xmp: false,
            xmp_exposure: false,
            gpu: false,
            keep_override: None,
            include_raw: st.include_raw,
            skip_processed: true,
        };
        match crate::score::run_score_job_in_workspace(&source, &paths, &cfg, &opts, &|ev| match ev {
            crate::score::ScoreEvent::Info(s) => job.log(format!("[score] {s}")),
            crate::score::ScoreEvent::Progress { done, total } => job.progress(done, total),
            crate::score::ScoreEvent::Finished { total_jpg, hits, misses } => job.log(format!(
                "[score] 分析完成: {total_jpg} 张 JPG（缓存命中 {hits}，新分析 {misses}）"
            )),
        }) {
            Ok(summary) => {
                // 快照热替换：新分数立即可见，无需重启服务
                let feedback = st.scene_feedback.read().unwrap().clone();
                match snapshot::build_snapshot_for_workspace(&source, &paths, &cfg, &cache_path,
                    &feedback, st.include_raw, true, &|_, _, _| {}) {
                    Ok(snap) => {
                        replace_snapshot(&st.snapshot, snap);
                        job.log("[review] 快照已刷新");
                    }
                    Err(err) => {
                        job.log(format!("[review] 快照刷新失败: {err:#}"));
                    }
                }
                let summary = format!(
                    "完成：{} 张 JPG（命中 {}，新分析 {}，失败 {}，RAW 无映射 {}）→ {}",
                    summary.total_jpg,
                    summary.hits,
                    summary.misses,
                    summary.failed_jpgs.len(),
                    summary.unmapped_arw,
                    summary.csv_path.display()
                );
                job.log(format!(
                    "[score] {summary}（耗时 {:.1} 秒）",
                    started.elapsed().as_secs_f64()
                ));
                job.finish(summary);
            }
            Err(err) => {
                job.log(format!(
                    "[score] 任务失败（耗时 {:.1} 秒）: {err:#}",
                    started.elapsed().as_secs_f64()
                ));
                job.fail(format!("{err:#}"));
            }
        }
    });
    StatusCode::ACCEPTED.into_response()
}

fn default_all() -> String {
    "all".to_string()
}

fn default_true() -> bool {
    true
}

#[derive(Deserialize, Default)]
struct ExportMinScores {
    #[serde(default)]
    total: f64,
    #[serde(default)]
    sharpness: f64,
    #[serde(default)]
    exposure: f64,
    #[serde(default)]
    noise: f64,
    #[serde(default)]
    composition: f64,
    #[serde(default)]
    aesthetic: f64,
}

#[derive(Deserialize, Default)]
struct ExportFilterPayload {
    #[serde(default)]
    search: String,
    #[serde(default)]
    stars: Vec<u8>,
    #[serde(default = "default_all")]
    keep: String,
    #[serde(default = "default_all")]
    group: String,
    #[serde(default)]
    faces: bool,
    #[serde(default = "default_all")]
    flag: String,
    #[serde(default)]
    min: ExportMinScores,
}

#[derive(Deserialize)]
struct ExportRequest {
    #[serde(default)]
    filter: ExportFilterPayload,
    #[serde(default)]
    subdir: Option<String>,
    #[serde(default)]
    external_target_id: Option<String>,
    #[serde(default)]
    layout: crate::export::ExportLayout,
    #[serde(default = "default_true")]
    include_jpg: bool,
    #[serde(default = "default_true")]
    include_raw: bool,
    #[serde(default = "default_true")]
    include_xmp: bool,
    #[serde(default)]
    xmp_exposure: bool,
}

#[cfg(any(windows, test))]
const APPROVED_EXPORT_CAP: usize = 32;

fn resolve_export_options(
    state: &AppState,
    req: &ExportRequest,
) -> std::result::Result<crate::export::ExportOptions, Response> {
    let target = if let Some(id) = req
        .external_target_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if !state.local_desktop {
            return Err(
                (StatusCode::FORBIDDEN, "非本机桌面模式仅允许导出到相册内子目录").into_response(),
            );
        }
        let approved = state.approved_export_dirs.lock().unwrap();
        let Some((_, path)) = approved.iter().find(|(token, _)| token == id) else {
            return Err(
                (StatusCode::BAD_REQUEST, "目标目录未授权或已过期，请重新选择文件夹").into_response(),
            );
        };
        crate::export::ExportTarget::ExternalDir(path.clone())
    } else {
        let subdir = req.subdir.as_deref().unwrap_or("Selected").trim();
        crate::export::ExportTarget::AlbumSubdir(PathBuf::from(subdir))
    };

    Ok(crate::export::ExportOptions {
        target,
        layout: req.layout,
        include_jpg: req.include_jpg,
        include_raw: req.include_raw && state.include_raw,
        include_xmp: req.include_xmp,
        xmp_exposure: req.xmp_exposure,
    })
}

fn photo_matches_filter(
    photo: &snapshot::PhotoJson,
    filter: &ExportFilterPayload,
    flag_filter: crate::export::FlagFilter,
) -> bool {
    let search = filter.search.trim().to_lowercase();
    if !search.is_empty() && !photo.filename.to_lowercase().contains(&search) {
        return false;
    }
    if !flag_filter.matches(photo.flag) {
        return false;
    }
    if !filter.stars.is_empty() && !photo.stars.is_some_and(|s| filter.stars.contains(&s)) {
        return false;
    }
    let group = photo.burst.as_ref().map(|b| b.group).unwrap_or(0);
    match filter.keep.as_str() {
        "keep" => {
            // 非连拍单张（group == 0）始终视为保留；连拍组仅保留有效 keep == true 的帧
            if group > 0 && !photo.burst.as_ref().is_some_and(|b| b.keep) {
                return false;
            }
        }
        "drop" => {
            if !(group > 0 && photo.burst.as_ref().is_some_and(|b| !b.keep)) {
                return false;
            }
        }
        _ => {}
    }
    if filter.group == "burst" && group == 0 {
        return false;
    }
    if filter.group == "single" && group != 0 {
        return false;
    }
    if let Some(target_gid) = filter.group.strip_prefix("group:") {
        if group == 0 || group.to_string() != target_gid {
            return false;
        }
    }
    if filter.faces && photo.faces == 0 {
        return false;
    }
    match &photo.scores {
        None => {
            let has_score_floor = filter.min.total > 0.0
                || filter.min.sharpness > 0.0
                || filter.min.exposure > 0.0
                || filter.min.noise > 0.0
                || filter.min.composition > 0.0
                || filter.min.aesthetic > 0.0;
            if has_score_floor {
                return false;
            }
        }
        Some(s) => {
            if s.total < filter.min.total
                || s.sharpness < filter.min.sharpness
                || s.exposure < filter.min.exposure
                || s.noise < filter.min.noise
                || s.composition < filter.min.composition
                || s.aesthetic < filter.min.aesthetic
            {
                return false;
            }
        }
    }
    true
}

fn replace_snapshot(slot: &RwLock<Snapshot>, mut next: Snapshot) {
    let mut guard = slot.write().unwrap();
    next.version = guard.version.saturating_add(1);
    *guard = next;
}

#[derive(Clone)]
struct ExpectedPhotoIdentity {
    jpg_rel: String,
    jpg_fp: Option<String>,
    raw_rel: Option<String>,
    raw_fp: Option<String>,
}

fn verify_export_identities(
    source: &WorkspaceSource,
    expected: &[ExpectedPhotoIdentity],
    include_raw: bool,
) -> std::result::Result<(), String> {
    for item in expected {
        if let Some(expected_fp) = item.jpg_fp.as_deref() {
            let actual = crate::decision::photo_fingerprint_for_source(source, false, &item.jpg_rel)
                .map_err(|_| {
                    format!(
                        "检测到源文件已变化或不可读（{}），当前快照已过期，请点击右上角「重启服务」或重新打开工作区后再导出",
                        item.jpg_rel
                    )
                })?;
            if actual != expected_fp {
                return Err(format!(
                    "检测到源文件已变化或被替换（{}），当前快照已过期，请点击右上角「重启服务」或重新打开工作区后再导出",
                    item.jpg_rel
                ));
            }
        }
        if include_raw {
            if let (Some(raw_rel), Some(expected_raw_fp)) =
                (item.raw_rel.as_deref(), item.raw_fp.as_deref())
            {
                let actual_raw =
                    crate::decision::photo_fingerprint_for_source(source, true, raw_rel).map_err(|_| {
                        format!(
                            "检测到配对 RAW 文件已变化或不可读（{}），当前快照已过期，请点击右上角「重启服务」或重新打开工作区后再导出",
                            raw_rel
                        )
                    })?;
                if actual_raw != expected_raw_fp {
                    return Err(format!(
                        "检测到配对 RAW 文件已变化或被替换（{}），当前快照已过期，请点击右上角「重启服务」或重新打开工作区后再导出",
                        raw_rel
                    ));
                }
            }
        }
    }
    Ok(())
}

fn verify_jpg_identity_for_mutation(
    source: &WorkspaceSource,
    rel: &str,
    expected_fp: Option<&str>,
) -> std::result::Result<String, Response> {
    let actual = crate::decision::photo_fingerprint_for_source(source, false, rel).map_err(|_| {
        (
            StatusCode::CONFLICT,
            format!("检测到源文件已变化或不可读（{rel}），当前快照已过期，请重启服务或重新打开工作区后再操作"),
        )
            .into_response()
    })?;
    if let Some(expected) = expected_fp {
        if actual != expected {
            return Err((
                StatusCode::CONFLICT,
                format!("检测到源文件已变化或被替换（{rel}），当前快照已过期，请重启服务或重新打开工作区后再操作"),
            )
                .into_response());
        }
    }
    Ok(actual)
}

fn build_export_items_from_snapshot(
    root: &Path,
    snap: &Snapshot,
    filter: &ExportFilterPayload,
    flag_filter: crate::export::FlagFilter,
) -> (
    Vec<crate::export::ExportPhotoItem>,
    Vec<String>,
    Vec<ExpectedPhotoIdentity>,
) {
    let all_sources: Vec<String> = snap.photos.iter().map(|p| p.path.clone()).collect();
    let mut raw_by_pair: HashMap<&str, &snapshot::PhotoJson> = HashMap::new();
    for p in snap.photos.iter().filter(|p| p.is_raw) {
        raw_by_pair.entry(p.pair_id.as_str()).or_insert(p);
    }

    let mut items = Vec::new();
    let mut identities = Vec::new();
    for photo in snap.photos.iter().filter(|p| !p.is_raw) {
        if !photo_matches_filter(photo, filter, flag_filter) {
            continue;
        }

        let paired_raw = raw_by_pair.get(photo.pair_id.as_str()).copied();
        let raw_rel = paired_raw.map(|r| r.path.clone());
        identities.push(ExpectedPhotoIdentity {
            jpg_rel: photo.path.clone(),
            jpg_fp: photo.content_fingerprint.clone(),
            raw_rel: raw_rel.clone(),
            raw_fp: paired_raw.and_then(|r| r.content_fingerprint.clone()),
        });
        let xmp_meta = photo.scores.as_ref().map(|s| {
            let entry = crate::scan::PhotoEntry {
                path: root.join(&photo.path).display().to_string(),
                filename: photo.filename.clone(),
                extension: photo.ext.clone(),
                is_raw: false,
                has_pair: photo.has_pair,
                pair_id: photo.pair_id.clone(),
                date_time_original: photo.datetime.clone(),
                iso: photo.iso.clone(),
                f_number: photo.f_number.clone(),
                shutter_speed: photo.shutter.clone(),
                focal_length: photo.focal.clone(),
                sharpness_score: format!("{:.1}", s.sharpness),
                exposure_score: format!("{:.1}", s.exposure),
                noise_score: format!("{:.1}", s.noise),
                composition_score: format!("{:.1}", s.composition),
                aesthetic_score: format!("{:.1}", s.aesthetic),
                total_score: format!("{:.1}", s.total),
                faces: photo.faces.to_string(),
                burst_group: photo.burst.as_ref().map(|b| b.group.to_string()).unwrap_or_else(|| "0".into()),
                burst_rank: photo
                    .burst
                    .as_ref()
                    .filter(|b| b.size > 0)
                    .map(|b| b.rank.to_string())
                    .unwrap_or_default(),
                burst_keep: photo
                    .burst
                    .as_ref()
                    .filter(|b| b.size > 0)
                    .map(|b| b.keep.to_string())
                    .unwrap_or_default(),
                ..Default::default()
            };
            crate::export::ExportXmpMeta {
                entry,
                scores: crate::score::PixelScores {
                    sharpness: s.sharpness,
                    exposure: s.exposure,
                    noise: s.noise,
                    composition: s.composition,
                    aesthetic: s.aesthetic,
                },
                total: s.total,
                rating: photo.stars.unwrap_or(3),
                suggested_ev: s.suggested_ev,
            }
        });
        items.push(crate::export::ExportPhotoItem {
            pair_id: photo.pair_id.clone(),
            jpg_rel: Some(photo.path.clone()),
            raw_rel,
            xmp_meta,
            manual_rating: if photo.scores.is_none() { photo.stars } else { None },
        });
    }
    (items, all_sources, identities)
}

/// POST /api/export/pick-folder：仅本机 Windows 桌面回环模式可用，调用系统原生目录弹窗授权导出目标。
async fn export_pick_folder(
    State(state): State<Arc<AppState>>,
    axum::Json(_): axum::Json<serde_json::Value>,
) -> Response {
    if !state.local_desktop {
        return (StatusCode::NOT_FOUND, "当前运行环境不支持系统文件夹选择器").into_response();
    }
    #[cfg(windows)]
    {
        let initial_dir = crate::workspace::load_workspace_meta(&state.paths)
            .and_then(|m| m.last_export_dir)
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .unwrap_or_else(|| state.root.clone());
        let picked = match tokio::task::spawn_blocking(move || {
            rfd::FileDialog::new()
                .set_title("选择导出目标文件夹")
                .set_directory(&initial_dir)
                .pick_folder()
        })
        .await
        {
            Ok(p) => p,
            Err(err) => return internal_error(err.into()),
        };
        let Some(path) = picked else {
            return json_response(serde_json::json!({ "cancelled": true }));
        };
        let normalized = match normal_photo_root(&path) {
            Ok(p) => p,
            Err(err) => return (StatusCode::BAD_REQUEST, format!("{err:#}")).into_response(),
        };
        if !state.source.is_dual() {
            match crate::storage::relative_under_album(&state.root, &normalized) {
                Ok(Some(rel)) if !rel.as_os_str().is_empty() => {
                    let subdir = rel.to_string_lossy().replace('\\', "/");
                    return json_response(serde_json::json!({
                        "cancelled": false,
                        "in_album": true,
                        "subdir": subdir,
                        "display_path": normalized.display().to_string(),
                    }));
                }
                Ok(Some(_)) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        "导出目录不能是照片根目录本身，请选择其他目录或使用子目录（如 Selected）",
                    )
                        .into_response();
                }
                Ok(None) => {}
                Err(err) => return (StatusCode::BAD_REQUEST, format!("{err:#}")).into_response(),
            }
        }
        let mut entropy = [0u8; 16];
        if let Err(err) = getrandom::fill(&mut entropy) {
            return internal_error(anyhow::anyhow!("{err}"));
        }
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
        let token = URL_SAFE_NO_PAD.encode(entropy);
        {
            let mut approved = state.approved_export_dirs.lock().unwrap();
            if approved.len() >= APPROVED_EXPORT_CAP {
                approved.remove(0);
            }
            approved.push((token.clone(), normalized.clone()));
        }
        json_response(serde_json::json!({
            "cancelled": false,
            "in_album": false,
            "target_id": token,
            "display_path": normalized.display().to_string(),
        }))
    }
    #[cfg(not(windows))]
    {
        (StatusCode::NOT_FOUND, "当前平台不支持系统文件夹选择器").into_response()
    }
}

/// POST /api/export/preview：只读预演导出清单（不创建任何目录或文件）。
async fn export_preview(
    State(state): State<Arc<AppState>>,
    axum::Json(req): axum::Json<ExportRequest>,
) -> Response {
    if state.loading.load(Ordering::Acquire) {
        return (StatusCode::CONFLICT, "照片仍在加载，请稍后再预览导出").into_response();
    }
    let Some(flag_filter) = crate::export::FlagFilter::parse(&req.filter.flag) else {
        return (StatusCode::BAD_REQUEST, "无效的选片标记筛选条件").into_response();
    };
    let opts = match resolve_export_options(&state, &req) {
        Ok(o) => o,
        Err(resp) => return resp,
    };
    let (items, all_sources, expected_identities) = {
        let guard = state.snapshot.read().unwrap();
        build_export_items_from_snapshot(&state.root, &guard, &req.filter, flag_filter)
    };
    let source = state.source.clone();
    match tokio::task::spawn_blocking(move || {
        if let Err(msg) = verify_export_identities(&source, &expected_identities, opts.include_raw) {
            return Err((StatusCode::CONFLICT, msg));
        }
        crate::export::plan_export_workspace(&source, &items, &all_sources, &opts)
            .map(|plan| plan.preview)
            .map_err(|err| (StatusCode::BAD_REQUEST, format!("{err:#}")))
    })
    .await
    {
        Ok(Ok(preview)) => json_response(serde_json::to_value(preview).unwrap_or_default()),
        Ok(Err((status, msg))) => (status, msg).into_response(),
        Err(err) => internal_error(err.into()),
    }
}

/// POST /api/export/run：启动后台导出任务（与评分、写操作、服务关闭严格互斥）。
async fn export_run(
    State(state): State<Arc<AppState>>,
    axum::Json(req): axum::Json<ExportRequest>,
) -> Response {
    if state.loading.load(Ordering::Acquire) {
        return (StatusCode::CONFLICT, "照片仍在加载，请稍后再导出").into_response();
    }
    let Some(flag_filter) = crate::export::FlagFilter::parse(&req.filter.flag) else {
        return (StatusCode::BAD_REQUEST, "无效的选片标记筛选条件").into_response();
    };
    let opts = match resolve_export_options(&state, &req) {
        Ok(o) => o,
        Err(resp) => return resp,
    };
    // 先占坑再读快照，彻底消除与进行中的改星/连拍覆盖写入之间的 TOCTOU 窗口
    let Some(cancel_flag) = state.job.begin_export() else {
        return (StatusCode::CONFLICT, "已有评分或导出任务在运行，请等待完成").into_response();
    };
    let (items, all_sources, expected_identities) = {
        let guard = state.snapshot.read().unwrap();
        build_export_items_from_snapshot(&state.root, &guard, &req.filter, flag_filter)
    };
    if items.is_empty() {
        state.job.abort_start();
        return (StatusCode::BAD_REQUEST, "当前筛选条件下没有可导出的照片").into_response();
    }

    let check_source = state.source.clone();
    let check_identities = expected_identities.clone();
    let include_raw_for_check = opts.include_raw;
    match tokio::task::spawn_blocking(move || {
        verify_export_identities(&check_source, &check_identities, include_raw_for_check)
    })
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(msg)) => {
            state.job.abort_start();
            return (StatusCode::CONFLICT, msg).into_response();
        }
        Err(err) => {
            state.job.abort_start();
            return internal_error(err.into());
        }
    }

    let job = Arc::clone(&state.job);
    let source = state.source.clone();
    let paths = state.paths.clone();
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        job.log("[export] 正在校验源文件并规划导出清单…");
        if let Err(msg) = verify_export_identities(&source, &expected_identities, opts.include_raw) {
            job.log(format!("[export] {msg}"));
            job.fail(msg);
            return;
        }
        let plan = match crate::export::plan_export_workspace(&source, &items, &all_sources, &opts) {
            Ok(p) => p,
            Err(err) => {
                job.log(format!("[export] 规划失败: {err:#}"));
                job.fail(format!("导出失败: {err:#}"));
                return;
            }
        };
        match crate::export::run_export(&plan, &cancel_flag, &|ev| match ev {
            crate::export::ExportEvent::Info(s) => job.log(format!("[export] {s}")),
            crate::export::ExportEvent::Progress {
                done,
                total,
                bytes_done,
                bytes_total,
                current_file,
            } => job.export_progress(done, total, bytes_done, bytes_total, current_file),
        }) {
            Ok(summary) => {
                job.set_last_export_dir(summary.target_path.clone());
                if !summary.in_album {
                    let _ = crate::workspace::update_last_export_dir(
                        &paths,
                        &source,
                        &summary.target_path,
                    );
                }
                let mb = summary.bytes_written as f64 / (1024.0 * 1024.0);
                let prefix = if summary.cancelled { "已取消" } else { "导出完成" };
                let conflict_note = if summary.conflicts.is_empty() {
                    String::new()
                } else {
                    format!("，冲突 {}", summary.conflicts.len())
                };
                let text = format!(
                    "{prefix}：JPG {} / RAW {} / XMP {}（{:.1} MB，跳过未变 {}，保留既有 {}，失败 {}{conflict_note}）→ {}",
                    summary.jpg_copied,
                    summary.raw_copied,
                    summary.xmp_written,
                    mb,
                    summary.skipped_unchanged,
                    summary.protected_existing,
                    summary.failed,
                    summary.target_display,
                );
                job.log(format!(
                    "[export] {text}（耗时 {:.1} 秒）",
                    started.elapsed().as_secs_f64()
                ));
                if summary.failed > 0 && summary.jpg_copied + summary.raw_copied + summary.xmp_written == 0 {
                    job.fail(text);
                } else {
                    job.finish(text);
                }
            }
            Err(err) => {
                job.log(format!(
                    "[export] 任务失败（耗时 {:.1} 秒）: {err:#}",
                    started.elapsed().as_secs_f64()
                ));
                job.fail(format!("导出失败: {err:#}"));
            }
        }
    });
    StatusCode::ACCEPTED.into_response()
}

/// POST /api/export/cancel：请求安全取消正在进行的导出任务。
async fn export_cancel(
    State(state): State<Arc<AppState>>,
    axum::Json(_): axum::Json<serde_json::Value>,
) -> Response {
    if state.job.request_cancel() {
        json_response(serde_json::json!({ "ok": true }))
    } else {
        (StatusCode::CONFLICT, "当前没有可取消的导出任务").into_response()
    }
}

/// POST /api/export/open：仅本机 Windows 桌面回环模式可用，在资源管理器中打开最近一次完成的导出目录。
async fn export_open(
    State(state): State<Arc<AppState>>,
    axum::Json(_): axum::Json<serde_json::Value>,
) -> Response {
    if !state.local_desktop {
        return (StatusCode::NOT_FOUND, "当前运行环境不支持打开本地文件夹").into_response();
    }
    let Some(dir) = state.job.last_export_dir() else {
        return (StatusCode::NOT_FOUND, "尚未完成导出任务").into_response();
    };
    #[cfg(windows)]
    {
        match tokio::task::spawn_blocking(move || open_folder_in_explorer(&dir)).await {
            Ok(Ok(())) => json_response(serde_json::json!({ "ok": true })),
            Ok(Err(err)) => (StatusCode::BAD_REQUEST, format!("{err:#}")).into_response(),
            Err(err) => internal_error(err.into()),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = dir;
        (StatusCode::NOT_FOUND, "当前平台不支持打开资源管理器").into_response()
    }
}

#[cfg(windows)]
fn open_folder_in_explorer(dir: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};
    anyhow::ensure!(dir.is_dir(), "目标目录不存在: {}", dir.display());
    // 打开前同样校验非符号链接/重解析点
    let _ = Folder::open_or_create_explicit(dir, false)?;
    let verb: Vec<u16> = "explore\0".encode_utf16().collect();
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            wide.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    anyhow::ensure!(
        result as isize > 32,
        "无法在资源管理器中打开目录（错误 {}）",
        result as isize
    );
    Ok(())
}

#[derive(Deserialize)]
struct RateParams {
    p: String,
    /// 1~5 星
    stars: u8,
}

#[derive(Deserialize)]
struct BurstKeepParams {
    p: String,
    /// true=手动保留，false=手动舍弃，null=恢复自动建议
    keep: Option<bool>,
}

/// POST /api/burst/keep：保存单张照片的手动连拍保留决定（统一持久化到 decisions.sqlite）。
async fn burst_keep(State(state): State<Arc<AppState>>,
    axum::Json(p): axum::Json<BurstKeepParams>) -> Response {
    run_serialized_mutation(
        state,
        "评分任务进行中或服务正在关闭，暂不能修改连拍标记",
        move |state| {
            let (pair_id, rel_path, expected_fp) = {
                let guard = state.snapshot.read().unwrap();
                let Some(photo) = guard.photos.iter().find(|x| x.path == p.p && !x.is_raw) else {
                    return (StatusCode::NOT_FOUND, "照片不在快照中").into_response();
                };
                if !photo.burst.as_ref().is_some_and(|b| b.group > 0) {
                    return (StatusCode::BAD_REQUEST, "只有连拍组照片可以手动标记").into_response();
                }
                (
                    photo.pair_id.clone(),
                    photo.path.clone(),
                    photo.content_fingerprint.clone(),
                )
            };
            let actual_fp = match verify_jpg_identity_for_mutation(
                &state.source,
                &rel_path,
                expected_fp.as_deref(),
            ) {
                Ok(fp) => fp,
                Err(resp) => return resp,
            };
            let jpg_path = state.root.join(&rel_path);

            if let Err(err) =
                crate::decision::set_burst_keep_in(&state.paths.state_dir, &state.root, &jpg_path, p.keep)
            {
                return internal_error(err);
            }

            let mut guard = state.snapshot.write().unwrap();
            guard.version = guard.version.saturating_add(1);
            let version = guard.version;
            for photo in guard.photos.iter_mut().filter(|x| x.pair_id == pair_id) {
                if !photo.is_raw && photo.content_fingerprint.is_none() {
                    photo.content_fingerprint = Some(actual_fp.clone());
                }
                if let Some(burst) = &mut photo.burst {
                    burst.keep = p.keep.unwrap_or(burst.suggested_keep);
                    burst.manual_keep = p.keep;
                }
            }
            json_response(serde_json::json!({"ok": true, "keep": p.keep, "snapshot_version": version}))
        },
    )
    .await
}

#[derive(Deserialize)]
struct SceneFeedbackParams {
    p: String,
    selected_scene: String,
    #[serde(default)]
    note: String,
}

/// POST /api/scene-feedback：记录场景纠错与当前评分线索，供后续校准。
async fn scene_feedback_save(State(state): State<Arc<AppState>>,
    axum::Json(p): axum::Json<SceneFeedbackParams>) -> Response {
    if !scene_feedback::valid_scene(&p.selected_scene) {
        return (StatusCode::BAD_REQUEST, "请选择有效的场景").into_response();
    }
    if p.note.chars().count() > 1000 {
        return (StatusCode::BAD_REQUEST, "备注不能超过 1000 字").into_response();
    }
    run_serialized_mutation(
        state,
        "评分任务进行中或服务正在关闭，暂不能记录场景",
        move |state| {
            let record = {
                let guard = state.snapshot.read().unwrap();
                let Some(photo) = guard.photos.iter().find(|x| x.path == p.p && !x.is_raw) else {
                    return (StatusCode::NOT_FOUND, "照片不在快照中").into_response();
                };
                SceneFeedbackRecord {
                    photo_path: photo.path.clone(),
                    predicted_scene: photo.scene_hint.id.to_string(),
                    predicted_reason: photo.scene_hint.reason.clone(),
                    selected_scene: p.selected_scene,
                    note: p.note.trim().to_string(),
                    recorded_at_unix_ms: scene_feedback::now_unix_ms(),
                    faces: photo.faces,
                    analysis_mode: photo.analysis_mode.clone(),
                    scores: photo.scores.clone(),
                }
            };
            if let Err(err) =
                scene_feedback::append(&state.paths.state_dir, &state.scene_feedback_path, &record)
            {
                return internal_error(err);
            }
            state.scene_feedback.write().unwrap().insert(record.photo_path.clone(), record.clone());
            let mut guard = state.snapshot.write().unwrap();
            guard.version = guard.version.saturating_add(1);
            let version = guard.version;
            if let Some(photo) = guard.photos.iter_mut().find(|x| x.path == record.photo_path) {
                photo.scene_feedback = Some(record.clone());
            }
            json_response(serde_json::json!({"ok": true, "record": record, "snapshot_version": version}))
        },
    )
    .await
}

#[derive(Deserialize)]
struct FlagParams {
    p: String,
    flag: crate::decision::CurationFlag,
}

#[derive(Deserialize)]
struct BatchFlagParams {
    #[serde(default)]
    filter: Option<ExportFilterPayload>,
    #[serde(default)]
    paths: Option<Vec<String>>,
    #[serde(default)]
    snapshot_version: Option<u64>,
    flag: crate::decision::CurationFlag,
}

/// POST /api/flag：保存单张照片的人工选片标记（留用 / 未定 / 剔除）。
async fn set_flag(
    State(state): State<Arc<AppState>>,
    axum::Json(p): axum::Json<FlagParams>,
) -> Response {
    if state.loading.load(Ordering::Acquire) {
        return (StatusCode::CONFLICT, "照片仍在加载，请稍后再标记").into_response();
    }
    run_serialized_mutation(
        state,
        "评分任务进行中或服务正在关闭，暂不能修改选片标记",
        move |state| {
            let (pair_id, rel_path, expected_fp) = {
                let guard = state.snapshot.read().unwrap();
                let Some(photo) = guard.photos.iter().find(|x| x.path == p.p && !x.is_raw) else {
                    return (StatusCode::NOT_FOUND, "照片不在快照中").into_response();
                };
                (
                    photo.pair_id.clone(),
                    photo.path.clone(),
                    photo.content_fingerprint.clone(),
                )
            };
            let actual_fp = match verify_jpg_identity_for_mutation(
                &state.source,
                &rel_path,
                expected_fp.as_deref(),
            ) {
                Ok(fp) => fp,
                Err(resp) => return resp,
            };
            let jpg_path = state.root.join(&rel_path);
            if let Err(err) = crate::decision::set_flag_in(
                &state.paths.state_dir,
                &state.root,
                &jpg_path,
                p.flag,
            ) {
                return internal_error(err);
            }
            let mut guard = state.snapshot.write().unwrap();
            guard.version = guard.version.saturating_add(1);
            let version = guard.version;
            for photo in guard.photos.iter_mut().filter(|x| x.pair_id == pair_id) {
                if !photo.is_raw && photo.content_fingerprint.is_none() {
                    photo.content_fingerprint = Some(actual_fp.clone());
                }
                photo.flag = p.flag;
            }
            json_response(serde_json::json!({
                "ok": true,
                "path": p.p,
                "flag": p.flag,
                "snapshot_version": version,
            }))
        },
    )
    .await
}

/// POST /api/flag/batch：将用户确认的一批照片批量设为指定选片标记（校验快照版本与目标集合）。
async fn set_flag_batch(
    State(state): State<Arc<AppState>>,
    axum::Json(req): axum::Json<BatchFlagParams>,
) -> Response {
    if state.loading.load(Ordering::Acquire) {
        return (StatusCode::CONFLICT, "照片仍在加载，请稍后再批量标记").into_response();
    }
    if req.paths.is_none() && req.snapshot_version.is_none() {
        return (StatusCode::BAD_REQUEST, "批量标记请求必须提供已确认的照片列表或快照版本")
            .into_response();
    }
    let parsed_flag_filter = match &req.filter {
        Some(filter) => match crate::export::FlagFilter::parse(&filter.flag) {
            Some(parsed) => Some(parsed),
            None => return (StatusCode::BAD_REQUEST, "无效的选片标记筛选条件").into_response(),
        },
        None => None,
    };
    run_serialized_mutation(
        state,
        "评分任务进行中或服务正在关闭，暂不能批量修改选片标记",
        move |state| {
            let targets: Vec<(String, String, PathBuf, Option<String>)> = {
                let guard = state.snapshot.read().unwrap();
                if req.snapshot_version.is_some_and(|v| v != guard.version) {
                    return (
                        StatusCode::CONFLICT,
                        "照片状态已在其他标签页或任务中变化，请刷新列表后重新确认",
                    )
                        .into_response();
                }
                if let (Some(filter), Some(flag_filter)) = (&req.filter, parsed_flag_filter) {
                    let matched: Vec<(String, String, PathBuf, Option<String>)> = guard
                        .photos
                        .iter()
                        .filter(|p| !p.is_raw && photo_matches_filter(p, filter, flag_filter))
                        .map(|p| {
                            (
                                p.path.clone(),
                                p.pair_id.clone(),
                                state.root.join(&p.path),
                                p.content_fingerprint.clone(),
                            )
                        })
                        .collect();
                    if let Some(req_paths) = &req.paths {
                        let req_set: std::collections::HashSet<&str> =
                            req_paths.iter().map(String::as_str).collect();
                        let matched_set: std::collections::HashSet<&str> =
                            matched.iter().map(|(path, _, _, _)| path.as_str()).collect();
                        if req_set.len() != req_paths.len() || req_set != matched_set {
                            return (
                                StatusCode::CONFLICT,
                                "当前筛选命中的照片已变化，请刷新列表后重新确认",
                            )
                                .into_response();
                        }
                    }
                    matched
                } else if let Some(req_paths) = &req.paths {
                    let by_path: HashMap<&str, &snapshot::PhotoJson> = guard
                        .photos
                        .iter()
                        .filter(|p| !p.is_raw)
                        .map(|p| (p.path.as_str(), p))
                        .collect();
                    let mut seen = std::collections::HashSet::with_capacity(req_paths.len());
                    let mut selected = Vec::with_capacity(req_paths.len());
                    for path in req_paths {
                        if !seen.insert(path.as_str()) {
                            continue;
                        }
                        let Some(photo) = by_path.get(path.as_str()).copied() else {
                            return (
                                StatusCode::CONFLICT,
                                "所选照片已不在当前快照中，请刷新后重新确认",
                            )
                                .into_response();
                        };
                        selected.push((
                            photo.path.clone(),
                            photo.pair_id.clone(),
                            state.root.join(&photo.path),
                            photo.content_fingerprint.clone(),
                        ));
                    }
                    selected
                } else {
                    Vec::new()
                }
            };
            let matched_count = targets.len();
            if targets.is_empty() {
                let version = state.snapshot.read().unwrap().version;
                return json_response(serde_json::json!({
                    "ok": true,
                    "flag": req.flag,
                    "matched_count": 0,
                    "updated_count": 0,
                    "failed_count": 0,
                    "updated": [],
                    "snapshot_version": version,
                }));
            }
            let mut verified_fps: HashMap<String, String> = HashMap::with_capacity(targets.len());
            for (rel, _, _, expected_fp) in &targets {
                let actual_fp = match verify_jpg_identity_for_mutation(
                    &state.source,
                    rel,
                    expected_fp.as_deref(),
                ) {
                    Ok(fp) => fp,
                    Err(resp) => return resp,
                };
                verified_fps.insert(rel.clone(), actual_fp);
            }
            let jpg_paths: Vec<PathBuf> = targets.iter().map(|(_, _, full, _)| full.clone()).collect();
            let outcome = match crate::decision::set_flags_batch_in(
                &state.paths.state_dir,
                &state.root,
                &jpg_paths,
                req.flag,
            ) {
                Ok(out) => out,
                Err(err) => return internal_error(err),
            };
            let updated_set: std::collections::HashSet<&str> =
                outcome.updated.iter().map(String::as_str).collect();
            let updated_pairs: std::collections::HashSet<String> = targets
                .iter()
                .filter(|(rel, _, _, _)| updated_set.contains(rel.as_str()))
                .map(|(_, pair_id, _, _)| pair_id.clone())
                .collect();
            let mut guard = state.snapshot.write().unwrap();
            guard.version = guard.version.saturating_add(1);
            let version = guard.version;
            for photo in guard
                .photos
                .iter_mut()
                .filter(|x| updated_pairs.contains(&x.pair_id))
            {
                if !photo.is_raw && photo.content_fingerprint.is_none() {
                    if let Some(fp) = verified_fps.get(&photo.path) {
                        photo.content_fingerprint = Some(fp.clone());
                    }
                }
                photo.flag = req.flag;
            }
            json_response(serde_json::json!({
                "ok": true,
                "flag": req.flag,
                "matched_count": matched_count,
                "updated_count": outcome.updated.len(),
                "failed_count": outcome.failed.len(),
                "updated": outcome.updated,
                "snapshot_version": version,
            }))
        },
    )
    .await
}

/// POST /api/rate：UI 内改星——持久化到本机工作区 `decisions.sqlite`（源目录零写入；导出时合并进目标 XMP）。
async fn rate(State(state): State<Arc<AppState>>,
    axum::Json(p): axum::Json<RateParams>) -> Response {
    if !(1..=5).contains(&p.stars) {
        return (StatusCode::BAD_REQUEST, "星级必须是 1~5").into_response();
    }
    if state.loading.load(Ordering::Acquire) {
        return (StatusCode::CONFLICT, "照片仍在加载，请稍后再改星").into_response();
    }
    run_serialized_mutation(
        state,
        "评分任务进行中或服务正在关闭，暂不能改星",
        move |state| {
            let (pair_id, rel_path, expected_fp) = {
                let guard = state.snapshot.read().unwrap();
                let Some(photo) = guard.photos.iter().find(|x| x.path == p.p && !x.is_raw) else {
                    return (StatusCode::NOT_FOUND, "照片不在快照中").into_response();
                };
                (
                    photo.pair_id.clone(),
                    photo.path.clone(),
                    photo.content_fingerprint.clone(),
                )
            };
            let actual_fp = match verify_jpg_identity_for_mutation(
                &state.source,
                &rel_path,
                expected_fp.as_deref(),
            ) {
                Ok(fp) => fp,
                Err(resp) => return resp,
            };
            let jpg_path = state.root.join(&rel_path);
            if let Err(err) = crate::decision::set_in(&state.paths.state_dir, &state.root, &jpg_path, p.stars) {
                return internal_error(err);
            }
            let mut guard = state.snapshot.write().unwrap();
            guard.version = guard.version.saturating_add(1);
            let version = guard.version;
            for photo in guard.photos.iter_mut().filter(|x| x.pair_id == pair_id) {
                if !photo.is_raw && photo.content_fingerprint.is_none() {
                    photo.content_fingerprint = Some(actual_fp.clone());
                }
                photo.stars = Some(p.stars);
                photo.rating_source = Some("manual".into());
            }
            let warnings: Vec<String> = Vec::new();
            json_response(serde_json::json!({
                "ok": true,
                "stars": p.stars,
                "warnings": warnings,
                "snapshot_version": version,
            }))
        },
    )
    .await
}

/// GET /api/config：当前可编辑配置值（文件不存在时为内置默认）
async fn config_get(State(state): State<Arc<AppState>>) -> Response {
    let state_dir = state.paths.state_dir.clone();
    let config_path = state.config_path.clone();
    let explicit = state.config_explicit;
    match tokio::task::spawn_blocking(move || {
        load_review_config_in(&state_dir, explicit.then_some(config_path.as_path()))
            .map(|(cfg, exists)| (config_path, cfg, exists))
    }).await {
        Ok(Ok((path, cfg, exists))) => json_response(serde_json::json!({
            "path": path.display().to_string(),
            "exists": exists,
            "values": config_edit::ConfigValues::from_cfg(&cfg),
        })),
        Ok(Err(err)) => internal_error(err),
        Err(err) => internal_error(err.into()),
    }
}

/// POST /api/config：保存编辑值（toml_edit 保留注释；非法值拒绝写盘）
async fn config_save(
    State(state): State<Arc<AppState>>,
    axum::Json(values): axum::Json<config_edit::ConfigValues>,
) -> Response {
    run_serialized_mutation(
        state,
        "评分任务进行中或服务正在关闭，暂不能保存配置",
        move |state| match config_edit::save_in(&state.paths.state_dir, &state.config_path, &values) {
            Ok(()) => {
                let note = "已保存；下一次 UI 跑批生效";
                json_response(serde_json::json!({ "ok": true, "note": note }))
            }
            Err(err) => (StatusCode::BAD_REQUEST, format!("{err:#}")).into_response(),
        },
    )
    .await
}

fn json_response(v: serde_json::Value) -> Response {
    (
        AppendHeaders([(header::CONTENT_TYPE, "application/json")]),
        serde_json::to_vec(&v).unwrap_or_default(),
    )
        .into_response()
}

/// Only an exact indexed JPG identity is accepted, then the file is opened
/// through the album directory without following replaced path components.
fn resolve_jpeg(state: &AppState, p: &str) -> std::result::Result<std::fs::File, Response> {
    let allowed = {
        let snapshot = state.snapshot.read().unwrap();
        snapshot.photos.binary_search_by(|photo| photo.path.as_str().cmp(p)).ok()
            .is_some_and(|i| {
                let photo = &snapshot.photos[i];
                !photo.is_raw && matches!(photo.ext.as_str(), "jpg" | "jpeg")
            })
    };
    if !allowed {
        return Err(StatusCode::NOT_FOUND.into_response());
    }
    match state
        .source
        .file_ref_for_photo(false, &state.source.photo_full(false, p))
        .and_then(|file| file.open_read())
    {
        Ok(Some(file)) => Ok(file),
        Ok(None) => Err(StatusCode::NOT_FOUND.into_response()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound =>
            Err(StatusCode::NOT_FOUND.into_response()),
        Err(_) => Err(StatusCode::FORBIDDEN.into_response()),
    }
}

fn internal_error(err: anyhow::Error) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")).into_response()
}


#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request};
    use tower::ServiceExt;

    const HOST: &str = "127.0.0.1:8787";
    const ORIGIN: &str = "http://127.0.0.1:8787";

    fn test_request(method: Method, url: &str, origin: Option<&str>, session: Option<&str>, json: Option<serde_json::Value>) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(url).header(header::HOST, HOST);
        if let Some(origin) = origin { builder = builder.header(header::ORIGIN, origin); }
        if let Some(session) = session { builder = builder.header(header::AUTHORIZATION, format!("Bearer {session}")); }
        let body = if let Some(json) = json {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(json.to_string())
        } else { Body::empty() };
        builder.body(body).unwrap()
    }

    #[test]
    fn photo_root_outputs_share_one_hidden_state_directory() {
        let root = Path::new("photos");
        let paths = photo_root_paths(root);
        assert_eq!(paths.state_dir, root.join(".firstcut"));
        assert_eq!(paths.cache, paths.state_dir.join("cache.sqlite"));
        assert_eq!(paths.report, paths.state_dir.join("report.csv"));
        assert_eq!(paths.scan_report, paths.state_dir.join("scan-report.csv"));
        assert_eq!(paths.config, paths.state_dir.join("config.toml"));
    }

    #[test]
    fn preferences_keep_defaults_when_loading_older_partial_file() {
        let dir = std::env::temp_dir().join(format!("firstcut_prefs_{}", std::process::id()));
        let state_dir = dir.join(".firstcut");
        std::fs::create_dir_all(&state_dir).unwrap();
        assert!(load_ui_preferences(&dir).include_raw);
        assert_eq!(load_ui_preferences(&dir).histogram_sample_percent, 10);
        std::fs::write(state_dir.join("ui-preferences.json"), r#"{"include_raw":false}"#).unwrap();
        let preferences = load_ui_preferences(&dir);
        assert!(!preferences.include_raw);
        assert_eq!(preferences.histogram_sample_percent, 10);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn photo_root_uses_cache_compatible_windows_path() {
        let root = std::env::temp_dir().join(format!("firstcut_path_identity_{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let selected = normal_photo_root(&root).unwrap();
        assert!(!selected.to_string_lossy().starts_with(r"\\?\"));
        let canonical = root.canonicalize().unwrap();
        assert_eq!(selected.canonicalize().unwrap(), canonical);
        let verbatim = canonical;
        assert_eq!(normal_photo_root(&verbatim).unwrap(), selected);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn ui_control_checks_origin_and_waits_for_scoring() {
        let control = Arc::new(UiControl {
            action: Mutex::new(None), notify: Notify::new(), session_id: 1,
        });
        let state = Arc::new(AppState {
            source: WorkspaceSource::Single { root: PathBuf::new() },
            paths: WorkspacePaths::in_state_dir(PathBuf::new()),
            root: PathBuf::new(), cache_path: PathBuf::new(), config_path: PathBuf::new(),
            config_explicit: false, scene_feedback: RwLock::new(HashMap::new()),
            scene_feedback_path: PathBuf::new(),
            image_slots: Arc::new(Semaphore::new(2)), thumb_slots: Arc::new(Semaphore::new(1)),
            snapshot: RwLock::new(Snapshot::empty(&PathBuf::new(), &ScoreConfig::default())),
            loading: AtomicBool::new(false), load_error: Mutex::new(None),
            load_progress: Mutex::new(LoadProgress { phase: String::new(), done: 0, total: 0 }),
            include_raw: true,
            local_desktop: false,
            approved_export_dirs: Mutex::new(Vec::new()),
            last_export_target: None,
            job: Arc::new(JobState::new()),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            control: Some(Arc::clone(&control)),
        });
        let access = Arc::new(access::Access::new(Ipv4Addr::LOCALHOST.into(), 8787, None, None).unwrap());
        let url = access.url();
        let (_, key) = url.split_once("#access=").unwrap();
        let app = router(state.clone(), access);
        for path in ["/api/photos", "/api/config", "/api/job", "/thumb", "/image"] {
            let response = app.clone().oneshot(test_request(Method::GET, path, None, None, None)).await.unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
            assert_eq!(response.headers()[header::CACHE_CONTROL], "private, no-store");
        }
        let login = test_request(Method::POST, "/api/session", Some(ORIGIN), None,
            Some(serde_json::json!({"key": key})));
        let response = app.clone().oneshot(login).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key(header::SET_COOKIE));
        let body = axum::body::to_bytes(response.into_body(), 1024).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let session = value["session"].as_str().unwrap();

        state.loading.store(true, Ordering::Release);
        let request = test_request(Method::GET, "/api/photos", None, Some(session), None);
        assert_eq!(app.clone().oneshot(request).await.unwrap().status(), StatusCode::SERVICE_UNAVAILABLE);
        state.loading.store(false, Ordering::Release);
        let request = test_request(Method::GET, "/api/photos", None, Some(session), None);
        assert_eq!(app.clone().oneshot(request).await.unwrap().status(), StatusCode::OK);

        // In-process policy assertions: requests missing the required origin
        // must stop before entering any of the mutation handlers.
        for path in [
            "/api/control",
            "/api/score/run",
            "/api/export/pick-folder",
            "/api/export/preview",
            "/api/export/run",
            "/api/export/cancel",
            "/api/export/open",
            "/api/rate",
            "/api/flag",
            "/api/flag/batch",
            "/api/burst/keep",
            "/api/config",
            "/api/scene-feedback",
            "/api/ui-settings",
        ] {
            let request = test_request(Method::POST, path, None, Some(session), None);
            assert_eq!(app.clone().oneshot(request).await.unwrap().status(), StatusCode::FORBIDDEN, "{path}");
        }
        assert!(!state.job.is_running());
        assert!(control.action.lock().unwrap().is_none());

        assert!(state.job.begin());
        let request = test_request(Method::POST, "/api/control", Some(ORIGIN), Some(session),
            Some(serde_json::json!({"action": "exit"})));
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        state.job.finish("完成".into());
        let request = test_request(Method::POST, "/api/control", Some(ORIGIN), Some(session),
            Some(serde_json::json!({"action": "restart"})));
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(*control.action.lock().unwrap(), Some(UiAction::Restart));
    }

    #[tokio::test]
    async fn jpeg_whitelist_blocks_arw() {
        // 直接测 resolve_jpeg 的白名单逻辑（不依赖真实服务）
        let fixture = tempfile::tempdir().unwrap();
        let dir = fixture.path().to_path_buf();
        std::fs::create_dir_all(dir.join("d")).unwrap();
        std::fs::write(dir.join("d/a.jpg"), b"jpeg").unwrap();
        std::fs::write(dir.join("d/b.ARW"), b"raw").unwrap();
        let snapshot = snapshot::build_snapshot(&dir, &ScoreConfig::default(),
            &dir.join("missing.sqlite"), &HashMap::new()).unwrap();
        // A second file on disk must not be served until the catalogue is refreshed.
        std::fs::write(dir.join("d/unlisted.jpg"), b"unlisted").unwrap();
        let state = AppState {
            source: WorkspaceSource::Single { root: dir.clone() },
            paths: WorkspacePaths::in_state_dir(dir.join(".firstcut")),
            root: dir.clone(),
            cache_path: dir.join("c.sqlite"),
            config_path: dir.join("firstcut.toml"),
            config_explicit: true,
            scene_feedback: RwLock::new(HashMap::new()),
            scene_feedback_path: scene_feedback::store_path(&dir),
            image_slots: Arc::new(Semaphore::new(2)), thumb_slots: Arc::new(Semaphore::new(1)),
            snapshot: RwLock::new(snapshot),
            job: Arc::new(JobState::new()),
            loading: AtomicBool::new(false), load_error: Mutex::new(None),
            load_progress: Mutex::new(LoadProgress { phase: String::new(), done: 0, total: 0 }),
            include_raw: true,
            local_desktop: false,
            approved_export_dirs: Mutex::new(Vec::new()),
            last_export_target: None,
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            control: None,
        };
        let ok = resolve_jpeg(&state, "d/a.jpg");
        assert!(ok.is_ok(), "jpg 应放行");
        let raw = resolve_jpeg(&state, "d/b.ARW");
        assert!(raw.is_err(), "非 jpg 应 403");
        let missing = resolve_jpeg(&state, "d/none.jpg");
        assert!(missing.is_err(), "不存在的路径应 403（canonicalize 失败）");
        assert!(resolve_jpeg(&state, "d/unlisted.jpg").is_err());
    }

    #[tokio::test]
    async fn manual_burst_keep_persists_and_can_restore_suggestion() {
        let dir = std::env::temp_dir().join(format!("firstcut_burst_override_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("scene")).unwrap();
        std::fs::write(dir.join("scene/a.jpg"), b"dummy-a").unwrap();
        let photo = snapshot::PhotoJson {
            path: "scene/a.jpg".into(), filename: "a.jpg".into(), ext: "jpg".into(),
            is_raw: false, has_pair: false, pair_id: "a".into(), content_fingerprint: None, datetime: String::new(),
            iso: String::new(), f_number: String::new(), shutter: String::new(), focal: String::new(),
            scores: None, stars: None, rating_source: None, analysis_mode: None, faces: 0,
            scene_hint: snapshot::SceneHintJson { id: "unknown", label: "未识别", reason: String::new() },
            scene_feedback: None,
            flag: crate::decision::CurationFlag::Unflagged,
            burst: Some(snapshot::BurstJson { group: 7, size: 1, rank: 1, keep: true,
                suggested_keep: true, manual_keep: None, pose_cluster: 1 }),
        };
        let state = Arc::new(AppState {
            source: WorkspaceSource::Single { root: dir.clone() },
            paths: WorkspacePaths::in_state_dir(dir.join(".firstcut")),
            root: dir.clone(), cache_path: dir.join("cache.sqlite"), config_path: dir.join("config.toml"),
            config_explicit: false,
            scene_feedback: RwLock::new(HashMap::new()), scene_feedback_path: scene_feedback::store_path(&dir),
            image_slots: Arc::new(Semaphore::new(2)), thumb_slots: Arc::new(Semaphore::new(1)),
            snapshot: RwLock::new(Snapshot {
                version: 1,
                root: dir.display().to_string(),
                source_mode: "single".into(),
                jpg_root: dir.display().to_string(),
                raw_root: None,
                weights: snapshot::WeightsJson { sharpness: 0.3, exposure: 0.25, noise: 0.15,
                    composition: 0.15, aesthetic: 0.15 },
                photos: vec![photo],
            }),
            job: Arc::new(JobState::new()),
            loading: AtomicBool::new(false), load_error: Mutex::new(None),
            load_progress: Mutex::new(LoadProgress { phase: String::new(), done: 0, total: 0 }),
            include_raw: true,
            local_desktop: false,
            approved_export_dirs: Mutex::new(Vec::new()),
            last_export_target: None,
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            control: None,
        });
        let response = burst_keep(State(Arc::clone(&state)), axum::Json(BurstKeepParams {
            p: "scene/a.jpg".into(), keep: Some(false),
        })).await;
        assert_eq!(response.status(), StatusCode::OK);
        let saved = crate::decision::load_burst_overrides(&dir).unwrap();
        assert_eq!(saved.get("scene/a.jpg"), Some(&false));
        {
            let guard = state.snapshot.read().unwrap();
            let burst = guard.photos[0].burst.as_ref().unwrap();
            assert!(!burst.keep);
            assert_eq!(burst.manual_keep, Some(false));
        }

        let response = burst_keep(State(Arc::clone(&state)), axum::Json(BurstKeepParams {
            p: "scene/a.jpg".into(), keep: None,
        })).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!crate::decision::load_burst_overrides(&dir).unwrap().contains_key("scene/a.jpg"));
        let guard = state.snapshot.read().unwrap();
        let burst = guard.photos[0].burst.as_ref().unwrap();
        assert!(burst.keep, "清除手动标记应恢复自动建议");
        assert_eq!(burst.manual_keep, None);
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn scene_feedback_endpoint_records_actual_score_context() {
        let dir = std::env::temp_dir().join(format!("firstcut_scene_endpoint_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let feedback_path = scene_feedback::store_path(&dir);
        let photo = snapshot::PhotoJson {
            path: "a.jpg".into(), filename: "a.jpg".into(), ext: "jpg".into(),
            is_raw: false, has_pair: false, pair_id: "a".into(), content_fingerprint: None, datetime: String::new(),
            iso: String::new(), f_number: String::new(), shutter: String::new(), focal: String::new(),
            scores: Some(snapshot::ScoresJson { sharpness: 40.0, exposure: 60.0, noise: 80.0,
                composition: 50.0, aesthetic: 70.0, total: 57.5, suggested_ev: None }),
            stars: Some(3), rating_source: None, analysis_mode: Some("ai".into()), faces: 1,
            burst: None, scene_hint: snapshot::SceneHintJson { id: "portrait", label: "人像候选",
                reason: "检测到 1 张人脸".into() }, scene_feedback: None,
            flag: crate::decision::CurationFlag::Unflagged,
        };
        let state = Arc::new(AppState {
            source: WorkspaceSource::Single { root: dir.clone() },
            paths: WorkspacePaths::in_state_dir(dir.join(".firstcut")),
            root: dir.clone(), cache_path: dir.join("cache.sqlite"), config_path: dir.join("config.toml"),
            config_explicit: false,
            scene_feedback: RwLock::new(HashMap::new()), scene_feedback_path: feedback_path.clone(),
            image_slots: Arc::new(Semaphore::new(2)), thumb_slots: Arc::new(Semaphore::new(1)),
            snapshot: RwLock::new(Snapshot {
                version: 1,
                root: dir.display().to_string(),
                source_mode: "single".into(),
                jpg_root: dir.display().to_string(),
                raw_root: None,
                weights: snapshot::WeightsJson { sharpness: 0.3, exposure: 0.25, noise: 0.15,
                    composition: 0.15, aesthetic: 0.15 },
                photos: vec![photo],
            }),
            job: Arc::new(JobState::new()),
            loading: AtomicBool::new(false), load_error: Mutex::new(None),
            load_progress: Mutex::new(LoadProgress { phase: String::new(), done: 0, total: 0 }),
            include_raw: true,
            local_desktop: false,
            approved_export_dirs: Mutex::new(Vec::new()),
            last_export_target: None,
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            control: None,
        });
        let response = scene_feedback_save(State(Arc::clone(&state)),
            axum::Json(SceneFeedbackParams { p: "a.jpg".into(), selected_scene: "stage".into(),
                note: "舞台灯光".into() })).await;
        assert_eq!(response.status(), StatusCode::OK);
        let saved = scene_feedback::load_latest(&dir, &feedback_path).unwrap();
        assert_eq!(saved["a.jpg"].predicted_scene, "portrait");
        assert_eq!(saved["a.jpg"].selected_scene, "stage");
        assert_eq!(saved["a.jpg"].scores.as_ref().unwrap().total, 57.5);
        assert_eq!(state.snapshot.read().unwrap().photos[0].scene_feedback.as_ref().unwrap().note, "舞台灯光");

        let response = scene_feedback_save(State(state),
            axum::Json(SceneFeedbackParams { p: "a.jpg".into(), selected_scene: "invalid".into(),
                note: String::new() })).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(std::fs::read_to_string(&feedback_path).unwrap().lines().count(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn concurrent_rate_requests_serialize_without_lock_errors() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        std::fs::create_dir_all(dir.join("scene")).unwrap();
        std::fs::write(dir.join("scene/a.jpg"), b"dummy-a").unwrap();
        std::fs::write(dir.join("scene/b.jpg"), b"dummy-b").unwrap();
        std::fs::write(dir.join("a.jpg"), b"jpg").unwrap();
        let photo = snapshot::PhotoJson {
            path: "a.jpg".into(), filename: "a.jpg".into(), ext: "jpg".into(),
            is_raw: false, has_pair: false, pair_id: "a".into(), content_fingerprint: None, datetime: String::new(),
            iso: String::new(), f_number: String::new(), shutter: String::new(), focal: String::new(),
            scores: Some(snapshot::ScoresJson { sharpness: 70.0, exposure: 75.0, noise: 80.0,
                composition: 60.0, aesthetic: 65.0, total: 70.5, suggested_ev: Some(0.33) }),
            stars: Some(3), rating_source: Some("algorithm".into()),
            analysis_mode: Some("pixel".into()), faces: 0, burst: None,
            scene_hint: snapshot::SceneHintJson { id: "unknown", label: "未识别", reason: String::new() },
            scene_feedback: None,
            flag: crate::decision::CurationFlag::Unflagged,
        };
        let state = Arc::new(AppState {
            source: WorkspaceSource::Single { root: dir.clone() },
            paths: WorkspacePaths::in_state_dir(dir.join(".firstcut")),
            root: dir.clone(), cache_path: dir.join(".firstcut/cache.sqlite"),
            config_path: dir.join(".firstcut/config.toml"),
            config_explicit: false,
            scene_feedback: RwLock::new(HashMap::new()), scene_feedback_path: scene_feedback::store_path(&dir),
            image_slots: Arc::new(Semaphore::new(2)), thumb_slots: Arc::new(Semaphore::new(1)),
            snapshot: RwLock::new(Snapshot {
                version: 1,
                root: dir.display().to_string(),
                source_mode: "single".into(),
                jpg_root: dir.display().to_string(),
                raw_root: None,
                weights: snapshot::WeightsJson { sharpness: 0.3, exposure: 0.25, noise: 0.15,
                    composition: 0.15, aesthetic: 0.15 },
                photos: vec![photo],
            }),
            job: Arc::new(JobState::new()),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            loading: AtomicBool::new(false), load_error: Mutex::new(None),
            load_progress: Mutex::new(LoadProgress { phase: String::new(), done: 0, total: 0 }),
            include_raw: true,
            local_desktop: false,
            approved_export_dirs: Mutex::new(Vec::new()),
            last_export_target: None,
            control: None,
        });
        let mut tasks = Vec::new();
        for stars in 1u8..=5 {
            let st = Arc::clone(&state);
            tasks.push(tokio::spawn(async move {
                rate(State(st), axum::Json(RateParams { p: "a.jpg".into(), stars })).await.status()
            }));
        }
        for task in tasks {
            assert_eq!(task.await.unwrap(), StatusCode::OK, "并发快速改星不应触发 SQLite 锁冲突 500");
        }
        let final_star = state.snapshot.read().unwrap().photos[0].stars.unwrap();
        let saved = crate::decision::load(&dir).unwrap();
        assert_eq!(saved.get("a.jpg").copied(), Some(final_star), "快照与决定库星级必须一致");
        assert!(!dir.join("a.xmp").exists(), "Web 改星不得向源目录写入 XMP");
    }

    #[tokio::test]
    async fn rate_preserves_existing_lightroom_exposure_on_disk() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("album");
        let export_dir = temp.path().join("Exported");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.jpg"), b"jpg").unwrap();
        let original_xmp = "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
             <x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n\
              <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n\
               <rdf:Description rdf:about=\"\"\n\
                 xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\"\n\
                 xmlns:firstcut=\"http://firstcut.local/ns/\"\n\
                 xmlns:crs=\"http://ns.adobe.com/camera-raw-settings/1.0/\"\n\
                 crs:HasSettings=\"True\" crs:ProcessVersion=\"11.0\" crs:Exposure2012=\"+1.75\" crs:Shadows2012=\"+35\">\n\
                 <xmp:Rating>3</xmp:Rating>\n\
                 <firstcut:sharpness>70.0</firstcut:sharpness>\n\
                 <firstcut:exposure>75.0</firstcut:exposure>\n\
                 <firstcut:noise>80.0</firstcut:noise>\n\
                 <firstcut:composition>60.0</firstcut:composition>\n\
                 <firstcut:aesthetic>65.0</firstcut:aesthetic>\n\
                 <firstcut:faces>0</firstcut:faces>\n\
                 <firstcut:total>70.5</firstcut:total>\n\
                 <firstcut:burstGroup>0</firstcut:burstGroup>\n\
                 <firstcut:burstRank></firstcut:burstRank>\n\
                 <firstcut:burstKeep></firstcut:burstKeep>\n\
               </rdf:Description>\n\
              </rdf:RDF>\n\
             </x:xmpmeta>\n\
             <?xpacket end=\"w\"?>";
        std::fs::write(dir.join("a.xmp"), original_xmp).unwrap();
        let photo = snapshot::PhotoJson {
            path: "a.jpg".into(), filename: "a.jpg".into(), ext: "jpg".into(),
            is_raw: false, has_pair: false, pair_id: "a".into(), content_fingerprint: None, datetime: String::new(),
            iso: String::new(), f_number: String::new(), shutter: String::new(), focal: String::new(),
            scores: Some(snapshot::ScoresJson { sharpness: 70.0, exposure: 75.0, noise: 80.0,
                composition: 60.0, aesthetic: 65.0, total: 70.5, suggested_ev: Some(0.33) }),
            stars: Some(3), rating_source: Some("algorithm".into()),
            analysis_mode: Some("pixel".into()), faces: 0, burst: None,
            scene_hint: snapshot::SceneHintJson { id: "unknown", label: "未识别", reason: String::new() },
            scene_feedback: None,
            flag: crate::decision::CurationFlag::Unflagged,
        };
        let state = Arc::new(AppState {
            source: WorkspaceSource::Single { root: dir.clone() },
            paths: WorkspacePaths::in_state_dir(dir.join(".firstcut")),
            root: dir.clone(), cache_path: dir.join(".firstcut/cache.sqlite"),
            config_path: dir.join(".firstcut/config.toml"),
            config_explicit: false,
            scene_feedback: RwLock::new(HashMap::new()), scene_feedback_path: scene_feedback::store_path(&dir),
            image_slots: Arc::new(Semaphore::new(2)), thumb_slots: Arc::new(Semaphore::new(1)),
            snapshot: RwLock::new(Snapshot {
                version: 1,
                root: dir.display().to_string(),
                source_mode: "single".into(),
                jpg_root: dir.display().to_string(),
                raw_root: None,
                weights: snapshot::WeightsJson { sharpness: 0.3, exposure: 0.25, noise: 0.15,
                    composition: 0.15, aesthetic: 0.15 },
                photos: vec![photo],
            }),
            job: Arc::new(JobState::new()),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            loading: AtomicBool::new(false), load_error: Mutex::new(None),
            load_progress: Mutex::new(LoadProgress { phase: String::new(), done: 0, total: 0 }),
            include_raw: true,
            local_desktop: false,
            approved_export_dirs: Mutex::new(Vec::new()),
            last_export_target: None,
            control: None,
        });

        let response = rate(State(Arc::clone(&state)), axum::Json(RateParams {
            p: "a.jpg".into(), stars: 5,
        })).await;
        assert_eq!(response.status(), StatusCode::OK);
        // 源目录 XMP 保持只读零写入
        assert_eq!(std::fs::read_to_string(dir.join("a.xmp")).unwrap(), original_xmp);

        // 导出时在内存中合并最新 5★ 到目标 XMP，同时保留原有 Lightroom 曝光
        let (items, all_sources, _) = {
            let guard = state.snapshot.read().unwrap();
            build_export_items_from_snapshot(&state.root, &guard, &ExportFilterPayload::default(), crate::export::FlagFilter::All)
        };
        let opts = crate::export::ExportOptions {
            target: crate::export::ExportTarget::ExternalDir(export_dir.clone()),
            ..Default::default()
        };
        let plan = crate::export::plan_export_workspace(&state.source, &items, &all_sources, &opts).unwrap();
        crate::export::run_export(&plan, &AtomicBool::new(false), &|_| {}).unwrap();
        let exported_sidecar = std::fs::read_to_string(export_dir.join("JPG/a.xmp")).unwrap();
        assert!(exported_sidecar.contains("<xmp:Rating>5</xmp:Rating>"));
        assert!(exported_sidecar.contains("crs:Exposure2012=\"+1.75\""), "导出合并不得覆盖已有 Lightroom 人工曝光");
        assert!(exported_sidecar.contains("crs:Shadows2012=\"+35\""));
        assert!(!exported_sidecar.contains("crs:Exposure2012=\"+0.33\""), "导出合并已有 firstcut 侧车时不应把建议 EV 写入 crs:Exposure2012");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_mutation_keeps_locks_and_updates_snapshot_before_next_write_or_job() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        std::fs::create_dir_all(dir.join("scene")).unwrap();
        std::fs::write(dir.join("scene/a.jpg"), b"dummy-a").unwrap();
        std::fs::write(dir.join("scene/b.jpg"), b"dummy-b").unwrap();
        let make_photo = |name: &str, pair: &str| snapshot::PhotoJson {
            path: format!("scene/{name}"), filename: name.into(), ext: "jpg".into(),
            is_raw: false, has_pair: false, pair_id: pair.into(), content_fingerprint: None, datetime: String::new(),
            iso: String::new(), f_number: String::new(), shutter: String::new(), focal: String::new(),
            scores: None, stars: None, rating_source: None, analysis_mode: None, faces: 0,
            scene_hint: snapshot::SceneHintJson { id: "unknown", label: "未识别", reason: String::new() },
            scene_feedback: None,
            flag: crate::decision::CurationFlag::Unflagged,
            burst: Some(snapshot::BurstJson { group: 1, size: 2, rank: 1, keep: true,
                suggested_keep: true, manual_keep: None, pose_cluster: 1 }),
        };
        let state = Arc::new(AppState {
            source: WorkspaceSource::Single { root: dir.clone() },
            paths: WorkspacePaths::in_state_dir(dir.join(".firstcut")),
            root: dir.clone(), cache_path: dir.join(".firstcut/cache.sqlite"),
            config_path: dir.join(".firstcut/config.toml"),
            config_explicit: false,
            scene_feedback: RwLock::new(HashMap::new()), scene_feedback_path: scene_feedback::store_path(&dir),
            image_slots: Arc::new(Semaphore::new(2)), thumb_slots: Arc::new(Semaphore::new(1)),
            snapshot: RwLock::new(Snapshot {
                version: 1,
                root: dir.display().to_string(),
                source_mode: "single".into(),
                jpg_root: dir.display().to_string(),
                raw_root: None,
                weights: snapshot::WeightsJson { sharpness: 0.3, exposure: 0.25, noise: 0.15,
                    composition: 0.15, aesthetic: 0.15 },
                photos: vec![make_photo("a.jpg", "a"), make_photo("b.jpg", "b")],
            }),
            job: Arc::new(JobState::new()),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            loading: AtomicBool::new(false), load_error: Mutex::new(None),
            load_progress: Mutex::new(LoadProgress { phase: String::new(), done: 0, total: 0 }),
            include_raw: true,
            local_desktop: false,
            approved_export_dirs: Mutex::new(Vec::new()),
            last_export_target: None,
            control: None,
        });

        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let entered_task = Arc::clone(&entered);
        let release_task = Arc::clone(&release);
        let st1 = Arc::clone(&state);

        // 模拟客户端在写操作落盘期间取消 HTTP 请求
        let cancelled_req = tokio::spawn(async move {
            run_serialized_mutation(st1, "busy", move |state| {
                entered_task.wait();
                release_task.wait();
                crate::decision::set_burst_keep(&state.root, &state.root.join("scene/a.jpg"), Some(false))
                    .unwrap();
                let mut guard = state.snapshot.write().unwrap();
                for photo in guard.photos.iter_mut().filter(|x| x.pair_id == "a") {
                    if let Some(burst) = &mut photo.burst {
                        burst.keep = false;
                        burst.manual_keep = Some(false);
                    }
                }
                json_response(serde_json::json!({ "ok": true }))
            })
            .await
        });

        // 等阻塞写入真正开始，再取消外层请求 future
        tokio::task::spawn_blocking(move || entered.wait()).await.unwrap();
        cancelled_req.abort();
        let err = cancelled_req.await.unwrap_err();
        assert!(err.is_cancelled(), "外层请求应已被取消");

        // 外层请求取消后，写锁与任务守卫必须仍然生效：跑批/关闭都不得提前启动
        assert!(!state.job.begin(), "后台写入未完成时不得启动评分任务");
        assert!(!state.job.begin_shutdown(), "后台写入未完成时不得进入关闭状态");
        assert!(state.write_lock.try_lock().is_err(), "后台写入未完成时写锁必须仍被持有");

        // 紧接着发起第二次真实 burst_keep 请求：它必须等第一次写完并更新内存/快照后再执行
        let second_req = tokio::spawn(burst_keep(
            State(Arc::clone(&state)),
            axum::Json(BurstKeepParams { p: "scene/b.jpg".into(), keep: Some(false) }),
        ));
        tokio::task::yield_now().await;
        assert!(!second_req.is_finished(), "后续写入必须等待已取消请求的落盘与快照更新完成");

        // 放行第一次写入，等待后续写入结束
        tokio::task::spawn_blocking(move || release.wait()).await.unwrap();
        let resp2 = second_req.await.unwrap();
        assert_eq!(resp2.status(), StatusCode::OK);

        // 两次决定必须都在 decisions.sqlite 与快照中完整保留
        let saved = crate::decision::load_burst_overrides(&dir).unwrap();
        assert_eq!(saved.get("scene/a.jpg"), Some(&false));
        assert_eq!(saved.get("scene/b.jpg"), Some(&false));
        let snap = state.snapshot.read().unwrap();
        assert_eq!(snap.photos[0].burst.as_ref().unwrap().manual_keep, Some(false));
        assert_eq!(snap.photos[1].burst.as_ref().unwrap().manual_keep, Some(false));
        drop(snap);

        // 写入全部完成后，评分任务可以正常启动
        assert!(state.job.begin(), "写入结束后应释放 MutationGuard");
    }

    #[tokio::test]
    async fn export_endpoints_enforce_security_filter_and_run_flow() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("album");
        std::fs::create_dir_all(dir.join("JPG")).unwrap();
        std::fs::create_dir_all(dir.join("RAW")).unwrap();
        std::fs::write(dir.join("JPG/single.jpg"), b"single-jpg").unwrap();
        std::fs::write(dir.join("RAW/single.ARW"), b"single-raw").unwrap();
        std::fs::write(dir.join("JPG/keep.jpg"), b"keep-jpg").unwrap();
        std::fs::write(dir.join("RAW/keep.ARW"), b"keep-raw").unwrap();
        std::fs::write(dir.join("JPG/drop.jpg"), b"drop-jpg").unwrap();
        std::fs::write(dir.join("RAW/drop.ARW"), b"drop-raw").unwrap();

        let make_photo = |rel: &str, name: &str, ext: &str, is_raw: bool, pair: &str, stars: u8, burst: Option<snapshot::BurstJson>| snapshot::PhotoJson {
            path: rel.into(), filename: name.into(), ext: ext.into(), is_raw, has_pair: true,
            pair_id: pair.into(), content_fingerprint: None, datetime: String::new(), iso: String::new(), f_number: String::new(),
            shutter: String::new(), focal: String::new(),
            scores: Some(snapshot::ScoresJson {
                sharpness: 80.0, exposure: 80.0, noise: 80.0, composition: 80.0, aesthetic: 80.0,
                total: 80.0, suggested_ev: None,
            }),
            stars: Some(stars), rating_source: Some("algorithm".into()),
            analysis_mode: Some("pixel".into()), faces: 0, burst,
            scene_hint: snapshot::SceneHintJson { id: "unknown", label: "未识别", reason: String::new() },
            scene_feedback: None,
            flag: crate::decision::CurationFlag::Unflagged,
        };

        let photos = vec![
            // 非连拍单张（group = 0）：在 keep="keep" 筛选下必须被保留！
            make_photo("JPG/single.jpg", "single.jpg", "jpg", false, "s", 5,
                Some(snapshot::BurstJson { group: 0, size: 0, rank: 0, keep: false, suggested_keep: false, manual_keep: None, pose_cluster: 0 })),
            make_photo("RAW/single.ARW", "single.ARW", "arw", true, "s", 5,
                Some(snapshot::BurstJson { group: 0, size: 0, rank: 0, keep: false, suggested_keep: false, manual_keep: None, pose_cluster: 0 })),
            // 连拍组保留帧
            make_photo("JPG/keep.jpg", "keep.jpg", "jpg", false, "k", 4,
                Some(snapshot::BurstJson { group: 1, size: 2, rank: 1, keep: true, suggested_keep: true, manual_keep: None, pose_cluster: 1 })),
            make_photo("RAW/keep.ARW", "keep.ARW", "arw", true, "k", 4,
                Some(snapshot::BurstJson { group: 1, size: 2, rank: 1, keep: true, suggested_keep: true, manual_keep: None, pose_cluster: 1 })),
            // 连拍组舍弃帧
            make_photo("JPG/drop.jpg", "drop.jpg", "jpg", false, "d", 4,
                Some(snapshot::BurstJson { group: 1, size: 2, rank: 2, keep: false, suggested_keep: false, manual_keep: None, pose_cluster: 1 })),
            make_photo("RAW/drop.ARW", "drop.ARW", "arw", true, "d", 4,
                Some(snapshot::BurstJson { group: 1, size: 2, rank: 2, keep: false, suggested_keep: false, manual_keep: None, pose_cluster: 1 })),
        ];

        let state = Arc::new(AppState {
            source: WorkspaceSource::Single { root: dir.clone() },
            paths: WorkspacePaths::in_state_dir(dir.join(".firstcut")),
            root: dir.clone(), cache_path: dir.join(".firstcut/cache.sqlite"),
            config_path: dir.join(".firstcut/config.toml"), config_explicit: false,
            scene_feedback: RwLock::new(HashMap::new()), scene_feedback_path: scene_feedback::store_path(&dir),
            image_slots: Arc::new(Semaphore::new(2)), thumb_slots: Arc::new(Semaphore::new(1)),
            snapshot: RwLock::new(Snapshot {
                version: 1,
                root: dir.display().to_string(),
                source_mode: "single".into(),
                jpg_root: dir.display().to_string(),
                raw_root: None,
                weights: snapshot::WeightsJson { sharpness: 0.3, exposure: 0.25, noise: 0.15, composition: 0.15, aesthetic: 0.15 },
                photos,
            }),
            job: Arc::new(JobState::new()),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            loading: AtomicBool::new(false), load_error: Mutex::new(None),
            load_progress: Mutex::new(LoadProgress { phase: String::new(), done: 0, total: 0 }),
            include_raw: true,
            local_desktop: false,
            approved_export_dirs: Mutex::new(Vec::new()),
            last_export_target: None,
            control: None,
        });

        // 1) 非桌面模式拒绝原生目录选择器、外部 target_id 与打开目录
        assert_eq!(
            export_pick_folder(State(Arc::clone(&state)), axum::Json(serde_json::json!({}))).await.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            export_open(State(Arc::clone(&state)), axum::Json(serde_json::json!({}))).await.status(),
            StatusCode::NOT_FOUND
        );
        let ext_req: ExportRequest = serde_json::from_value(serde_json::json!({
            "external_target_id": "forged-token",
            "filter": {}
        })).unwrap();
        assert_eq!(
            export_preview(State(Arc::clone(&state)), axum::Json(ext_req)).await.status(),
            StatusCode::FORBIDDEN
        );

        // 2) 预览：keep="keep" 应命中 非连拍单张(single) + 连拍保留(keep)，排除 drop
        let preview_req: ExportRequest = serde_json::from_value(serde_json::json!({
            "subdir": "Selected",
            "layout": "split_by_type",
            "include_jpg": true,
            "include_raw": true,
            "include_xmp": true,
            "filter": { "stars": [4, 5], "keep": "keep" }
        })).unwrap();
        let resp = export_preview(State(Arc::clone(&state)), axum::Json(preview_req)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let preview: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(preview["selected_photos"], 2, "非连拍单张 + 连拍保留帧共 2 张");
        assert_eq!(preview["jpg_to_copy"], 2);
        assert_eq!(preview["raw_to_copy"], 2);
        assert_eq!(preview["xmp_to_write"], 4);

        // 3) 执行导出并等待后台任务完成
        let run_req: ExportRequest = serde_json::from_value(serde_json::json!({
            "subdir": "Selected",
            "layout": "split_by_type",
            "include_jpg": true,
            "include_raw": true,
            "include_xmp": true,
            "filter": { "stars": [4, 5], "keep": "keep" }
        })).unwrap();
        let run_resp = export_run(State(Arc::clone(&state)), axum::Json(run_req)).await;
        assert_eq!(run_resp.status(), StatusCode::ACCEPTED);
        for _ in 0..100 {
            if !state.job.is_running() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(matches!(state.job.status(), JobStatus::Done { .. }));
        assert!(dir.join("Selected/JPG/single.jpg").is_file());
        assert!(dir.join("Selected/RAW/single.ARW").is_file());
        assert!(dir.join("Selected/JPG/keep.jpg").is_file());
        assert!(dir.join("Selected/RAW/keep.ARW").is_file());
        assert!(!dir.join("Selected/JPG/drop.jpg").exists());
        assert!(dir.join("Selected").join(crate::scan::EXPORT_MARKER_NAME).is_file());
    }

    #[tokio::test]
    async fn export_preview_rejects_raw_dir_when_raw_association_disabled() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("album");
        std::fs::create_dir_all(dir.join("JPG")).unwrap();
        std::fs::create_dir_all(dir.join("RAW")).unwrap();
        std::fs::write(dir.join("JPG/only.jpg"), b"jpg").unwrap();
        std::fs::write(dir.join("RAW/only.ARW"), b"raw").unwrap();

        let state = Arc::new(AppState {
            source: WorkspaceSource::Single { root: dir.clone() },
            paths: WorkspacePaths::in_state_dir(dir.join(".firstcut")),
            root: dir.clone(),
            cache_path: dir.join(".firstcut/cache.sqlite"),
            config_path: dir.join(".firstcut/config.toml"),
            config_explicit: false,
            scene_feedback: RwLock::new(HashMap::new()),
            scene_feedback_path: scene_feedback::store_path(&dir),
            image_slots: Arc::new(Semaphore::new(2)),
            thumb_slots: Arc::new(Semaphore::new(1)),
            snapshot: RwLock::new(Snapshot {
                version: 1,
                root: dir.display().to_string(),
                source_mode: "single".into(),
                jpg_root: dir.display().to_string(),
                raw_root: None,
                weights: snapshot::WeightsJson {
                    sharpness: 0.3,
                    exposure: 0.25,
                    noise: 0.15,
                    composition: 0.15,
                    aesthetic: 0.15,
                },
                photos: vec![snapshot::PhotoJson {
                    path: "JPG/only.jpg".into(),
                    filename: "only.jpg".into(),
                    ext: "jpg".into(),
                    is_raw: false,
                    has_pair: false,
                    pair_id: "only".into(),
                    content_fingerprint: None,
                    datetime: String::new(),
                    iso: String::new(),
                    f_number: String::new(),
                    shutter: String::new(),
                    focal: String::new(),
                    scores: None,
                    stars: Some(5),
                    rating_source: Some("manual".into()),
                    analysis_mode: None,
                    faces: 0,
                    burst: None,
                    scene_hint: snapshot::SceneHintJson {
                        id: "unknown",
                        label: "未识别",
                        reason: String::new(),
                    },
                    scene_feedback: None,
                    flag: crate::decision::CurationFlag::Unflagged,
                }],
            }),
            job: Arc::new(JobState::new()),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            loading: AtomicBool::new(false),
            load_error: Mutex::new(None),
            load_progress: Mutex::new(LoadProgress {
                phase: String::new(),
                done: 0,
                total: 0,
            }),
            include_raw: false,
            local_desktop: false,
            approved_export_dirs: Mutex::new(Vec::new()),
            last_export_target: None,
            control: None,
        });

        let req: ExportRequest = serde_json::from_value(serde_json::json!({
            "subdir": "RAW",
            "include_jpg": true,
            "include_raw": false,
            "include_xmp": false,
            "filter": {}
        }))
        .unwrap();
        let resp = export_preview(State(state), axum::Json(req)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(!dir.join("RAW").join(crate::scan::EXPORT_MARKER_NAME).exists());
    }

    #[tokio::test]
    async fn welcome_router_picks_validates_and_opens_single_and_dual_workspaces() {
        let temp = tempfile::tempdir().unwrap();
        let jpg_dir = temp.path().join("jpg_card");
        let raw_dir = temp.path().join("raw_card");
        std::fs::create_dir_all(&jpg_dir).unwrap();
        std::fs::create_dir_all(&raw_dir).unwrap();

        let state = Arc::new(WelcomeState {
            local_desktop: true,
            approved_picks: Mutex::new(Vec::new()),
            outcome: Mutex::new(None),
            notify: Notify::new(),
            session_id: 42,
        });
        let jpg_token = mint_pick_token(&state.approved_picks, jpg_dir.clone()).unwrap();
        let raw_token = mint_pick_token(&state.approved_picks, raw_dir.clone()).unwrap();

        // 1) 伪造 pick_id 必须被拒绝
        let bad_resp = welcome_open(
            State(Arc::clone(&state)),
            axum::Json(serde_json::from_value(serde_json::json!({
                "mode": "single",
                "root_pick_id": "forged-token"
            })).unwrap()),
        )
        .await;
        assert_eq!(bad_resp.status(), StatusCode::BAD_REQUEST);

        // 2) 双目录选同一个目录必须被拒绝
        let same_resp = welcome_open(
            State(Arc::clone(&state)),
            axum::Json(serde_json::from_value(serde_json::json!({
                "mode": "dual",
                "jpg_pick_id": jpg_token,
                "raw_pick_id": jpg_token
            })).unwrap()),
        )
        .await;
        assert_eq!(same_resp.status(), StatusCode::BAD_REQUEST);

        // 3) 合法双目录打开成功并写入 outcome
        let ok_resp = welcome_open(
            State(Arc::clone(&state)),
            axum::Json(serde_json::from_value(serde_json::json!({
                "mode": "dual",
                "jpg_pick_id": jpg_token,
                "raw_pick_id": raw_token
            })).unwrap()),
        )
        .await;
        assert_eq!(ok_resp.status(), StatusCode::OK);
        let outcome = state.outcome.lock().unwrap().take();
        assert!(matches!(outcome, Some(WelcomeOutcome::Open(WorkspaceSource::Dual { .. }))));
    }

    #[test]
    fn welcome_open_imports_legacy_ui_preferences_before_saving_include_raw() {
        let temp = tempfile::tempdir().unwrap();
        let album = temp.path().join("legacy_album");
        let legacy_dir = album.join(".firstcut");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        let legacy_prefs = legacy_dir.join("ui-preferences.json");
        std::fs::write(
            &legacy_prefs,
            br#"{"include_raw":true,"histogram_sample_percent":22}"#,
        )
        .unwrap();

        let source = WorkspaceSource::single(&album).unwrap();
        let paths = WorkspacePaths::in_state_dir(temp.path().join("local_state"));

        apply_welcome_single_preferences(&source, &paths, false);

        let migrated = load_ui_preferences_in(&paths.state_dir);
        assert!(!migrated.include_raw);
        assert_eq!(
            migrated.histogram_sample_percent, 22,
            "欢迎页保存 include_raw 前必须先导入旧版 ui-preferences.json，保留原直方图采样比例"
        );
        assert_eq!(
            std::fs::read_to_string(&legacy_prefs).unwrap(),
            r#"{"include_raw":true,"histogram_sample_percent":22}"#
        );
    }

    #[tokio::test]
    async fn flag_and_batch_flag_endpoints_persist_update_snapshot_and_filter_export() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("album");
        std::fs::create_dir_all(dir.join("JPG")).unwrap();
        std::fs::create_dir_all(dir.join("RAW")).unwrap();
        std::fs::write(dir.join("JPG/a.jpg"), b"jpg-a").unwrap();
        std::fs::write(dir.join("RAW/a.ARW"), b"raw-a").unwrap();
        std::fs::write(dir.join("JPG/b.jpg"), b"jpg-b").unwrap();
        std::fs::write(dir.join("RAW/b.ARW"), b"raw-b").unwrap();

        let make_photo = |rel: &str, name: &str, ext: &str, is_raw: bool, pair: &str| snapshot::PhotoJson {
            path: rel.into(),
            filename: name.into(),
            ext: ext.into(),
            is_raw,
            has_pair: true,
            pair_id: pair.into(),
            content_fingerprint: None,
            datetime: String::new(),
            iso: String::new(),
            f_number: String::new(),
            shutter: String::new(),
            focal: String::new(),
            scores: None,
            stars: None,
            rating_source: None,
            analysis_mode: None,
            faces: 0,
            burst: None,
            flag: crate::decision::CurationFlag::Unflagged,
            scene_hint: snapshot::SceneHintJson {
                id: "unknown",
                label: "未识别",
                reason: String::new(),
            },
            scene_feedback: None,
        };

        let state = Arc::new(AppState {
            source: WorkspaceSource::Single { root: dir.clone() },
            paths: WorkspacePaths::in_state_dir(temp.path().join("local_state")),
            root: dir.clone(),
            cache_path: temp.path().join("local_state/cache.sqlite"),
            config_path: temp.path().join("local_state/config.toml"),
            config_explicit: false,
            scene_feedback: RwLock::new(HashMap::new()),
            scene_feedback_path: temp.path().join("local_state/scene-feedback.jsonl"),
            image_slots: Arc::new(Semaphore::new(2)),
            thumb_slots: Arc::new(Semaphore::new(1)),
            snapshot: RwLock::new(Snapshot {
                version: 1,
                root: dir.display().to_string(),
                source_mode: "single".into(),
                jpg_root: dir.display().to_string(),
                raw_root: None,
                weights: snapshot::WeightsJson {
                    sharpness: 0.3,
                    exposure: 0.25,
                    noise: 0.15,
                    composition: 0.15,
                    aesthetic: 0.15,
                },
                photos: vec![
                    make_photo("JPG/a.jpg", "a.jpg", "jpg", false, "pa"),
                    make_photo("RAW/a.ARW", "a.ARW", "arw", true, "pa"),
                    make_photo("JPG/b.jpg", "b.jpg", "jpg", false, "pb"),
                    make_photo("RAW/b.ARW", "b.ARW", "arw", true, "pb"),
                ],
            }),
            job: Arc::new(JobState::new()),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            loading: AtomicBool::new(false),
            load_error: Mutex::new(None),
            load_progress: Mutex::new(LoadProgress {
                phase: String::new(),
                done: 0,
                total: 0,
            }),
            include_raw: true,
            local_desktop: false,
            approved_export_dirs: Mutex::new(Vec::new()),
            last_export_target: None,
            control: None,
        });

        // 1) 未评分照片可直接打星与单张标记
        let rate_resp = rate(
            State(Arc::clone(&state)),
            axum::Json(RateParams {
                p: "JPG/a.jpg".into(),
                stars: 5,
            }),
        )
        .await;
        assert_eq!(rate_resp.status(), StatusCode::OK);

        let flag_resp = set_flag(
            State(Arc::clone(&state)),
            axum::Json(FlagParams {
                p: "JPG/b.jpg".into(),
                flag: crate::decision::CurationFlag::Reject,
            }),
        )
        .await;
        assert_eq!(flag_resp.status(), StatusCode::OK);

        // 2) 按筛选批量将 5★ 照片设为留用（仅命中 JPG/a.jpg，同步到配对 RAW）
        let batch_req: BatchFlagParams = serde_json::from_value(serde_json::json!({
            "filter": { "stars": [5] },
            "paths": ["JPG/a.jpg"],
            "snapshot_version": 3,
            "flag": "pick"
        }))
        .unwrap();
        let batch_resp = set_flag_batch(State(Arc::clone(&state)), axum::Json(batch_req)).await;
        assert_eq!(batch_resp.status(), StatusCode::OK);
        let batch_body = axum::body::to_bytes(batch_resp.into_body(), 16 * 1024)
            .await
            .unwrap();
        let batch_json: serde_json::Value = serde_json::from_slice(&batch_body).unwrap();
        assert_eq!(batch_json["updated_count"], 1);
        {
            let snap = state.snapshot.read().unwrap();
            assert_eq!(snap.photos[0].flag, crate::decision::CurationFlag::Pick);
            assert_eq!(snap.photos[1].flag, crate::decision::CurationFlag::Pick);
            assert_eq!(snap.photos[2].flag, crate::decision::CurationFlag::Reject);
            assert_eq!(snap.photos[3].flag, crate::decision::CurationFlag::Reject);
        }

        // 3) 导出预览按 flag="pick" 仅命中 a，且为未评分但已打星的 a 生成 XMP
        let preview_req: ExportRequest = serde_json::from_value(serde_json::json!({
            "subdir": "Selected",
            "filter": { "flag": "pick" }
        }))
        .unwrap();
        let preview_resp = export_preview(State(Arc::clone(&state)), axum::Json(preview_req)).await;
        assert_eq!(preview_resp.status(), StatusCode::OK);
        let preview_body = axum::body::to_bytes(preview_resp.into_body(), 16 * 1024)
            .await
            .unwrap();
        let preview_json: serde_json::Value = serde_json::from_slice(&preview_body).unwrap();
        assert_eq!(preview_json["selected_photos"], 1);
        assert_eq!(preview_json["xmp_to_write"], 2);
        assert!(!dir.join(".firstcut").exists(), "源目录必须保持零写入");
    }

    #[tokio::test]
    async fn export_preview_and_run_reject_replaced_picked_photo_with_conflict() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("album");
        let state_dir = temp.path().join("local_state");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("sample.jpg"), b"photo-card-a-bytes").unwrap();

        let source = WorkspaceSource::single(&dir).unwrap();
        let paths = WorkspacePaths::in_state_dir(state_dir);
        paths.ensure_state_dir().unwrap();
        let cfg = ScoreConfig::default();
        let snap = snapshot::build_snapshot_for_workspace(
            &source,
            &paths,
            &cfg,
            &paths.cache,
            &HashMap::new(),
            true,
            true,
            &|_, _, _| {},
        )
        .unwrap();
        assert_eq!(snap.photos.len(), 1);
        assert!(snap.photos[0].content_fingerprint.is_some());

        let state = Arc::new(AppState {
            source,
            paths: paths.clone(),
            root: dir.clone(),
            cache_path: paths.cache.clone(),
            config_path: paths.config.clone(),
            config_explicit: false,
            scene_feedback: RwLock::new(HashMap::new()),
            scene_feedback_path: paths.scene_feedback.clone(),
            image_slots: Arc::new(Semaphore::new(2)),
            thumb_slots: Arc::new(Semaphore::new(1)),
            snapshot: RwLock::new(snap),
            job: Arc::new(JobState::new()),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            loading: AtomicBool::new(false),
            load_error: Mutex::new(None),
            load_progress: Mutex::new(LoadProgress {
                phase: String::new(),
                done: 0,
                total: 0,
            }),
            include_raw: true,
            local_desktop: false,
            approved_export_dirs: Mutex::new(Vec::new()),
            last_export_target: None,
            control: None,
        });

        let flag_resp = set_flag(
            State(Arc::clone(&state)),
            axum::Json(FlagParams {
                p: "sample.jpg".into(),
                flag: crate::decision::CurationFlag::Pick,
            }),
        )
        .await;
        assert_eq!(flag_resp.status(), StatusCode::OK);

        // 在同一挂载路径用同长度但不同内容的另一张照片替换 sample.jpg
        std::fs::write(dir.join("sample.jpg"), b"photo-card-b-bytes").unwrap();

        let preview_req: ExportRequest = serde_json::from_value(serde_json::json!({
            "subdir": "Selected",
            "filter": { "flag": "pick" }
        }))
        .unwrap();
        let preview_resp = export_preview(State(Arc::clone(&state)), axum::Json(preview_req)).await;
        assert_eq!(preview_resp.status(), StatusCode::CONFLICT);
        let preview_msg = String::from_utf8(
            axum::body::to_bytes(preview_resp.into_body(), 16 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(preview_msg.contains("重启服务") || preview_msg.contains("重新打开工作区"));

        let run_req: ExportRequest = serde_json::from_value(serde_json::json!({
            "subdir": "Selected",
            "filter": { "flag": "pick" }
        }))
        .unwrap();
        let run_resp = export_run(State(Arc::clone(&state)), axum::Json(run_req)).await;
        assert_eq!(run_resp.status(), StatusCode::CONFLICT);
        assert!(!dir.join("Selected").exists(), "源文件已被替换时不得导出任何文件");
    }

    #[tokio::test]
    async fn batch_flag_rejects_stale_tab_when_filtered_targets_swap_with_same_count() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("album");
        let state_dir = temp.path().join("local_state");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.jpg"), b"jpg-a").unwrap();
        std::fs::write(dir.join("b.jpg"), b"jpg-b").unwrap();

        let source = WorkspaceSource::single(&dir).unwrap();
        let paths = WorkspacePaths::in_state_dir(state_dir);
        paths.ensure_state_dir().unwrap();
        let cfg = ScoreConfig::default();
        let snap = snapshot::build_snapshot_for_workspace(
            &source,
            &paths,
            &cfg,
            &paths.cache,
            &HashMap::new(),
            true,
            true,
            &|_, _, _| {},
        )
        .unwrap();
        let state = Arc::new(AppState {
            source,
            paths: paths.clone(),
            root: dir.clone(),
            cache_path: paths.cache.clone(),
            config_path: paths.config.clone(),
            config_explicit: false,
            scene_feedback: RwLock::new(HashMap::new()),
            scene_feedback_path: paths.scene_feedback.clone(),
            image_slots: Arc::new(Semaphore::new(2)),
            thumb_slots: Arc::new(Semaphore::new(1)),
            snapshot: RwLock::new(snap),
            job: Arc::new(JobState::new()),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            loading: AtomicBool::new(false),
            load_error: Mutex::new(None),
            load_progress: Mutex::new(LoadProgress {
                phase: String::new(),
                done: 0,
                total: 0,
            }),
            include_raw: true,
            local_desktop: false,
            approved_export_dirs: Mutex::new(Vec::new()),
            last_export_target: None,
            control: None,
        });

        // 初始：a.jpg 为剔除（页签 1 看到 reject=[a.jpg]）
        assert_eq!(
            set_flag(
                State(Arc::clone(&state)),
                axum::Json(FlagParams {
                    p: "a.jpg".into(),
                    flag: crate::decision::CurationFlag::Reject,
                }),
            )
            .await
            .status(),
            StatusCode::OK
        );
        let tab1_version = state.snapshot.read().unwrap().version;

        // 页签 2：清除 a.jpg 标记，改为剔除 b.jpg（reject 命中数仍为 1 张）
        assert_eq!(
            set_flag(
                State(Arc::clone(&state)),
                axum::Json(FlagParams {
                    p: "a.jpg".into(),
                    flag: crate::decision::CurationFlag::Unflagged,
                }),
            )
            .await
            .status(),
            StatusCode::OK
        );
        assert_eq!(
            set_flag(
                State(Arc::clone(&state)),
                axum::Json(FlagParams {
                    p: "b.jpg".into(),
                    flag: crate::decision::CurationFlag::Reject,
                }),
            )
            .await
            .status(),
            StatusCode::OK
        );

        // 页签 1 携带旧快照版本与旧路径集合提交批量留用，必须返回 409
        let stale_with_version: BatchFlagParams = serde_json::from_value(serde_json::json!({
            "filter": { "flag": "reject" },
            "paths": ["a.jpg"],
            "snapshot_version": tab1_version,
            "flag": "pick"
        }))
        .unwrap();
        assert_eq!(
            set_flag_batch(State(Arc::clone(&state)), axum::Json(stale_with_version))
                .await
                .status(),
            StatusCode::CONFLICT
        );

        // 即使不传 snapshot_version，只要提交了确认的 paths=["a.jpg"]，也因目标集合已变而返回 409
        let stale_paths_only: BatchFlagParams = serde_json::from_value(serde_json::json!({
            "filter": { "flag": "reject" },
            "paths": ["a.jpg"],
            "flag": "pick"
        }))
        .unwrap();
        assert_eq!(
            set_flag_batch(State(Arc::clone(&state)), axum::Json(stale_paths_only))
                .await
                .status(),
            StatusCode::CONFLICT
        );

        let guard = state.snapshot.read().unwrap();
        assert_eq!(guard.photos[0].flag, crate::decision::CurationFlag::Unflagged);
        assert_eq!(guard.photos[1].flag, crate::decision::CurationFlag::Reject);
    }

    #[tokio::test]
    async fn web_export_merges_manual_rating_into_self_closing_external_xmp_preserving_crs_exposure() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("album");
        let state_dir = temp.path().join("local_state");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.jpg"), b"jpg-unscored").unwrap();
        let external_xmp = concat!(
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">",
            "<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">",
            "<rdf:Description rdf:about=\"\" ",
            "xmlns:crs=\"http://ns.adobe.com/camera-raw-settings/1.0/\" ",
            "crs:Exposure2012=\"+0.75\"/>",
            "</rdf:RDF></x:xmpmeta>",
        );
        std::fs::write(dir.join("a.xmp"), external_xmp).unwrap();

        let source = WorkspaceSource::single(&dir).unwrap();
        let paths = WorkspacePaths::in_state_dir(state_dir);
        paths.ensure_state_dir().unwrap();
        let snap = snapshot::build_snapshot_for_workspace(
            &source,
            &paths,
            &ScoreConfig::default(),
            &paths.cache,
            &HashMap::new(),
            true,
            true,
            &|_, _, _| {},
        )
        .unwrap();
        let state = Arc::new(AppState {
            source,
            paths: paths.clone(),
            root: dir.clone(),
            cache_path: paths.cache.clone(),
            config_path: paths.config.clone(),
            config_explicit: false,
            scene_feedback: RwLock::new(HashMap::new()),
            scene_feedback_path: paths.scene_feedback.clone(),
            image_slots: Arc::new(Semaphore::new(2)),
            thumb_slots: Arc::new(Semaphore::new(1)),
            snapshot: RwLock::new(snap),
            job: Arc::new(JobState::new()),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            loading: AtomicBool::new(false),
            load_error: Mutex::new(None),
            load_progress: Mutex::new(LoadProgress {
                phase: String::new(),
                done: 0,
                total: 0,
            }),
            include_raw: true,
            local_desktop: false,
            approved_export_dirs: Mutex::new(Vec::new()),
            last_export_target: None,
            control: None,
        });

        let rate_resp = rate(
            State(Arc::clone(&state)),
            axum::Json(RateParams {
                p: "a.jpg".into(),
                stars: 5,
            }),
        )
        .await;
        assert_eq!(rate_resp.status(), StatusCode::OK);

        let run_req: ExportRequest = serde_json::from_value(serde_json::json!({
            "subdir": "Selected",
            "filter": {}
        }))
        .unwrap();
        let run_resp = export_run(State(Arc::clone(&state)), axum::Json(run_req)).await;
        assert_eq!(run_resp.status(), StatusCode::ACCEPTED);
        for _ in 0..100 {
            if !state.job.is_running() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(matches!(state.job.status(), JobStatus::Done { .. }));

        let exported_xmp = std::fs::read_to_string(dir.join("Selected/JPG/a.xmp")).unwrap();
        let doc = roxmltree::Document::parse(&exported_xmp).unwrap();
        let desc = doc
            .descendants()
            .find(|n| n.has_tag_name(("http://www.w3.org/1999/02/22-rdf-syntax-ns#", "Description")))
            .unwrap();
        assert_eq!(
            desc.attribute(("http://ns.adobe.com/xap/1.0/", "Rating")),
            Some("5")
        );
        assert_eq!(
            desc.attribute(("http://ns.adobe.com/camera-raw-settings/1.0/", "Exposure2012")),
            Some("+0.75")
        );
        assert_eq!(std::fs::read_to_string(dir.join("a.xmp")).unwrap(), external_xmp);
    }
}
