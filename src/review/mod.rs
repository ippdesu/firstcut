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
    match state_file(root, "ui-preferences.json", false).and_then(|file| file.read(16 * 1024)) {
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

fn save_ui_preferences(root: &Path, preferences: &UiPreferences) -> Result<()> {
    state_file(root, "ui-preferences.json", true)?.write(&serde_json::to_vec_pretty(preferences)?)?;
    Ok(())
}

fn state_file(root: &Path, name: &str, create: bool) -> std::io::Result<FileRef> {
    Folder::open(root)?.state(create)?.file(name)
}

pub fn load_review_config(root: &Path, explicit: Option<&Path>) -> Result<(ScoreConfig, bool)> {
    let path = explicit.map(Path::to_path_buf).unwrap_or_else(|| photo_root_paths(root).config);
    let bytes = match FileRef::at(root, &path, false) {
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
    let absolute = root.canonicalize()?;
    #[cfg(windows)] {
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

struct UiControl {
    action: Mutex<Option<UiAction>>,
    notify: Notify,
    session_id: u64,
}

pub struct AppState {
    root: PathBuf,
    cache_path: PathBuf,
    /// UI 配置编辑的目标文件（--config 指定；缺省 <照片根>/.firstcut/config.toml）
    config_path: PathBuf,
    config_explicit: bool,
    /// 用户手动设置的连拍保留状态（照片相对路径 → 是否保留）
    burst_overrides: RwLock<HashMap<String, bool>>,
    burst_overrides_path: PathBuf,
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
    /// 全局单评分任务
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
    anyhow::ensure!(port != 0, "复核服务端口必须为 1–65535");
    let configured_key = match std::env::var("FIRSTCUT_ACCESS_TOKEN") {
        Ok(key) => Some(key),
        Err(std::env::VarError::NotPresent) => None,
        Err(err) => return Err(err.into()),
    };
    let access = Arc::new(access::Access::new(bind, port, public_url, configured_key.as_deref())?);
    Folder::open(root)?.state(true)?;
    let preferences = load_ui_preferences(root);
    serve_once(root, cfg, cache_path, bind, port, open_browser, config_path,
        false, preferences.include_raw, true, access)?;
    Ok(())
}

/// Windows 双击启动器：在同一进程中重启服务，退出时正常释放端口。
pub fn serve_ui(root: &Path, pick_root: impl Fn() -> Option<PathBuf>) -> Result<()> {
    let mut root = normal_photo_root(root)?;
    let mut open_browser = true;
    let bind = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let access = Arc::new(access::Access::new(bind, 8787, None, None)?);
    #[cfg(windows)]
    let _reopen_guard = UiReopenListener::start(access.url());
    loop {
        let paths = photo_root_paths(&root);
        Folder::open(&root)?.state(true)?;
        let cache_path = paths.cache;
        let (config, _) = load_review_config(&root, None)?;
        let preferences = load_ui_preferences(&root);
        let include_raw = preferences.include_raw;
        match serve_once(&root, &config, &cache_path,
            bind, 8787, open_browser, None,
            true, include_raw, true, access.clone())? {
            Some(UiAction::Restart) => open_browser = false,
            Some(UiAction::Choose) => {
                if let Some(next) = pick_root() {
                    root = normal_photo_root(&next)?;
                }
                open_browser = false;
            }
            Some(UiAction::Rawon) => {
                let mut preferences = load_ui_preferences(&root);
                preferences.include_raw = true;
                save_ui_preferences(&root, &preferences)?;
                open_browser = false;
            }
            Some(UiAction::Rawoff) => {
                let mut preferences = load_ui_preferences(&root);
                preferences.include_raw = false;
                save_ui_preferences(&root, &preferences)?;
                open_browser = false;
            }
            _ => return Ok(()),
        }
    }
}

fn serve_once(
    root: &Path,
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
    let burst_overrides_path = root.join(".firstcut").join("burst-overrides.json");
    let burst_overrides = load_burst_overrides(root, &burst_overrides_path)?;
    let scene_feedback_path = scene_feedback::store_path(root);
    let feedback = scene_feedback::load_latest(root, &scene_feedback_path)?;
    let initial_snapshot = if ui_control {
        Snapshot::empty(root, cfg)
    } else {
        build_review_snapshot(root, cfg, cache_path, &burst_overrides, &feedback,
            include_raw, skip_processed, &|_, _, _| {})?
    };
    let load_overrides = burst_overrides.clone();
    let load_feedback = feedback.clone();

    let config_explicit = config_path.is_some();
    let config_path = config_path.map(|p| p.to_path_buf())
        .unwrap_or_else(|| root.join(".firstcut").join("config.toml"));
    let control = ui_control.then(|| Arc::new(UiControl {
        action: Mutex::new(None),
        notify: Notify::new(),
        session_id: NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed),
    }));
    let state = Arc::new(AppState {
        root: root.to_path_buf(),
        cache_path: cache_path.to_path_buf(),
        config_path,
        config_explicit,
        burst_overrides: RwLock::new(burst_overrides),
        burst_overrides_path,
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
        job: Arc::new(JobState::with_log_file(root, root.join(".firstcut").join("review.log"))),
        write_lock: Arc::new(tokio::sync::Mutex::new(())),
        control: control.clone(),
    });
    let load_state = Arc::clone(&state);
    let load_root = root.to_path_buf();
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
                    build_review_snapshot(&load_root, &load_cfg, &load_cache,
                        &load_overrides, &load_feedback, include_raw, skip_processed, &on_progress)
                }));
                match result {
                    Ok(Ok(snapshot)) => *load_state.snapshot.write().unwrap() = snapshot,
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
    Router::new()
        .route("/", get(index))
        .route("/review.js", get(review_js))
        .route("/healthz", get(|| async { access::HEALTH_MARKER }))
        .route("/api/photos", get(photos))
        .route("/api/load", get(load_status))
        .route("/thumb", get(thumb))
        .route("/image", get(image))
        .route("/api/job", get(job_status))
        .route("/api/score/run", post(score_run))
        .route("/api/rate", post(rate))
        .route("/api/burst/keep", post(burst_keep))
        .route("/api/scene-feedback", post(scene_feedback_save))
        .route("/api/config", get(config_get).post(config_save))
        .route("/api/ui-settings", get(ui_settings_get).post(ui_settings_save))
        .route("/api/control", get(control_status).post(control_post))
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024))
        .with_state(state)
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

fn build_review_snapshot(
    root: &Path, cfg: &ScoreConfig, cache_path: &Path,
    overrides: &HashMap<String, bool>,
    feedback: &HashMap<String, SceneFeedbackRecord>,
    include_raw: bool,
    skip_processed: bool,
    progress: &dyn Fn(&str, usize, usize),
) -> Result<Snapshot> {
    quiet_log("[review] 正在构建快照（扫描 + 缓存）……");
    let snapshot = snapshot::build_snapshot_with_options(root, cfg, cache_path,
        overrides, feedback, include_raw, skip_processed, progress)?;
    let scored = snapshot.photos.iter().filter(|p| p.scores.is_some()).count();
    quiet_log(format!("[review] 快照就绪：{} 张照片（已评分 {}，未评分 {}）",
        snapshot.photos.len(), scored, snapshot.photos.len() - scored));
    Ok(snapshot)
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
        thumb::get_or_create(&state.root, &q.p, file).map_err(internal_error)
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

/// GET /api/job：当前任务状态 + 最近日志（前端 600ms 轮询）
async fn job_status(State(state): State<Arc<AppState>>) -> Response {
    let (status, log) = state.job.snapshot();
    let body = match status {
        JobStatus::Idle => serde_json::json!({ "state": "idle", "log": log }),
        JobStatus::Running { done, total } => {
            serde_json::json!({ "state": "running", "done": done, "total": total, "log": log })
        }
        JobStatus::Done { summary } => {
            serde_json::json!({ "state": "done", "summary": summary, "log": log })
        }
        JobStatus::Failed { error } => {
            serde_json::json!({ "state": "failed", "error": error, "log": log })
        }
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
        Some(control) => serde_json::json!({ "enabled": true, "session": control.session_id,
            "include_raw": state.include_raw }),
        None => serde_json::json!({ "enabled": false }),
    })
}

#[derive(Deserialize)]
struct UiSettingsRequest {
    histogram_sample_percent: u8,
}

async fn ui_settings_get(State(state): State<Arc<AppState>>) -> Response {
    let root = state.root.clone();
    match tokio::task::spawn_blocking(move || load_ui_preferences(&root)).await {
        Ok(preferences) => json_response(serde_json::json!({
            "histogram_sample_percent": preferences.histogram_sample_percent,
        })),
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
    let _write_guard = state.write_lock.lock().await;
    let Some(_mutation) = state.job.begin_mutation() else {
        return (StatusCode::CONFLICT, "评分任务进行中或服务正在关闭，暂不能保存设置").into_response();
    };
    let root = state.root.clone();
    let sample_percent = request.histogram_sample_percent;
    match tokio::task::spawn_blocking(move || -> Result<u8> {
        let mut preferences = load_ui_preferences(&root);
        preferences.histogram_sample_percent = sample_percent;
        save_ui_preferences(&root, &preferences)?;
        Ok(preferences.histogram_sample_percent)
    }).await {
        Ok(Ok(saved)) => json_response(serde_json::json!({
            "ok": true,
            "histogram_sample_percent": saved,
        })),
        Ok(Err(error)) => internal_error(error),
        Err(err) => internal_error(err.into()),
    }
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
    /// 跳过 AI 推理（纯像素快速预览）
    #[serde(default)]
    no_ai: bool,
}

/// POST /api/score/run：触发一次完整评分（单任务互斥；完成后热替换快照）
async fn score_run(State(state): State<Arc<AppState>>,
    axum::Json(params): axum::Json<ScoreRunParams>) -> Response {
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
    let root = state.root.clone();
    let cache_path = state.cache_path.clone();
    let config_path = state.config_path.clone();
    let xmp = params.xmp;
    let no_ai = params.no_ai;
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        job.log("[score] 任务开始");
        // 每次跑批都重新加载配置：UI 里的配置编辑保存后下一次跑批即生效
        let (cfg, config_exists) = match load_review_config(&root,
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
            "[score] 使用内置默认配置（照片根目录尚无 UI 配置）".into()
        });

        let opts = crate::score::ScoreJobOptions {
            output_csv: photo_root_paths(&root).report,
            raw_csv: false,
            cache_path: cache_path.clone(),
            no_cache: false,
            no_ai,
            xmp,
            gpu: false,
            keep_override: None,
            include_raw: st.include_raw,
            skip_processed: true,
        };
        match crate::score::run_score_job(&root, &cfg, &opts, &|ev| match ev {
            crate::score::ScoreEvent::Info(s) => job.log(format!("[score] {s}")),
            crate::score::ScoreEvent::Progress { done, total } => job.progress(done, total),
            crate::score::ScoreEvent::Finished { total_jpg, hits, misses } => job.log(format!(
                "[score] 分析完成: {total_jpg} 张 JPG（缓存命中 {hits}，新分析 {misses}）"
            )),
        }) {
            Ok(summary) => {
                // 快照热替换：新分数立即可见，无需重启服务
                let overrides = st.burst_overrides.read().unwrap().clone();
                let feedback = st.scene_feedback.read().unwrap().clone();
                match snapshot::build_snapshot_with_options(&root, &cfg, &cache_path,
                    &overrides, &feedback, st.include_raw, true, &|_, _, _| {}) {
                    Ok(snap) => {
                        *st.snapshot.write().unwrap() = snap;
                        job.log("[review] 快照已刷新");
                    }
                    Err(err) => {
                        job.log(format!("[review] 快照刷新失败: {err:#}"));
                    }
                }
                let summary = format!(
                    "完成：{} 张 JPG（命中 {}，新分析 {}，失败 {}，ARW 无映射 {}）→ {}",
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

/// POST /api/burst/keep：保存单张照片的手动连拍保留决定。
async fn burst_keep(State(state): State<Arc<AppState>>,
    axum::Json(p): axum::Json<BurstKeepParams>) -> Response {
    let _write_guard = state.write_lock.lock().await;
    let Some(_mutation) = state.job.begin_mutation() else {
        return (StatusCode::CONFLICT, "评分任务进行中或服务正在关闭，暂不能修改连拍标记").into_response();
    };
    let pair_id = {
        let guard = state.snapshot.read().unwrap();
        let Some(photo) = guard.photos.iter().find(|x| x.path == p.p && !x.is_raw) else {
            return (StatusCode::NOT_FOUND, "照片不在快照中").into_response();
        };
        if !photo.burst.as_ref().is_some_and(|b| b.group > 0) {
            return (StatusCode::BAD_REQUEST, "只有连拍组照片可以手动标记").into_response();
        }
        photo.pair_id.clone()
    };

    let mut next_overrides = state.burst_overrides.read().unwrap().clone();
    if let Some(keep) = p.keep { next_overrides.insert(p.p.clone(), keep); }
    else { next_overrides.remove(&p.p); }
    let bytes = match serde_json::to_vec_pretty(&next_overrides) {
        Ok(bytes) => bytes,
        Err(err) => return internal_error(err.into()),
    };
    let root = state.root.clone();
    let path = state.burst_overrides_path.clone();
    match tokio::task::spawn_blocking(move || {
        FileRef::at(&root, &path, true).and_then(|file| file.write(&bytes))
    }).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => return internal_error(err.into()),
        Err(err) => return internal_error(err.into()),
    }
    *state.burst_overrides.write().unwrap() = next_overrides;

    let mut guard = state.snapshot.write().unwrap();
    for photo in guard.photos.iter_mut().filter(|x| x.pair_id == pair_id) {
        if let Some(burst) = &mut photo.burst {
            burst.keep = p.keep.unwrap_or(burst.suggested_keep);
            burst.manual_keep = p.keep;
        }
    }
    json_response(serde_json::json!({"ok": true, "keep": p.keep}))
}

fn load_burst_overrides(root: &Path, path: &Path) -> Result<HashMap<String, bool>> {
    let bytes = FileRef::at(root, path, false)?.read(32 * 1024 * 1024)?;
    match bytes {
        Some(bytes) => Ok(serde_json::from_slice(&bytes)?),
        None => Ok(HashMap::new()),
    }
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
    let _write_guard = state.write_lock.lock().await;
    let Some(_mutation) = state.job.begin_mutation() else {
        return (StatusCode::CONFLICT, "评分任务进行中或服务正在关闭，暂不能记录场景").into_response();
    };
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
    let root = state.root.clone();
    let path = state.scene_feedback_path.clone();
    let write_record = record.clone();
    match tokio::task::spawn_blocking(move || {
        scene_feedback::append(&root, &path, &write_record)
    }).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => return internal_error(err),
        Err(err) => return internal_error(err.into()),
    }
    state.scene_feedback.write().unwrap().insert(record.photo_path.clone(), record.clone());
    let mut guard = state.snapshot.write().unwrap();
    if let Some(photo) = guard.photos.iter_mut().find(|x| x.path == record.photo_path) {
        photo.scene_feedback = Some(record.clone());
    }
    json_response(serde_json::json!({"ok": true, "record": record}))
}

/// POST /api/rate：UI 内改星——只改 `xmp:Rating`，firstcut 子分字段保留；
/// 他人侧车（无 firstcut 命名空间）不覆盖（沿用 write_sidecar 保护）
async fn rate(State(state): State<Arc<AppState>>,
    axum::Json(p): axum::Json<RateParams>) -> Response {
    if !(1..=5).contains(&p.stars) {
        return (StatusCode::BAD_REQUEST, "星级必须是 1~5").into_response();
    }
    let _write_guard = state.write_lock.lock().await;
    let Some(_mutation) = state.job.begin_mutation() else {
        return (StatusCode::CONFLICT, "评分任务进行中或服务正在关闭，暂不能改星").into_response();
    };
    // 从当前快照取该照片的分数（未评分照片没有可写子分，拒绝）
    let (scores, total, suggested_ev, pair_id, jpg_path, sidecar_entries) = {
        let guard = state.snapshot.read().unwrap();
        let Some(photo) = guard.photos.iter().find(|x| x.path == p.p && !x.is_raw) else {
            return (StatusCode::NOT_FOUND, "照片不在快照中").into_response();
        };
        let Some(s) = &photo.scores else {
            return (StatusCode::BAD_REQUEST, "该照片未评分，先跑一次评分再改星").into_response();
        };
        let px = crate::score::PixelScores {
            sharpness: s.sharpness,
            exposure: s.exposure,
            noise: s.noise,
            composition: s.composition,
            aesthetic: s.aesthetic,
        };
        // render_xmp 需要完整 PhotoEntry 字段；从快照视图重建
        let entry = crate::scan::PhotoEntry {
            path: state.root.join(&photo.path).display().to_string(),
            filename: photo.filename.clone(),
            extension: photo.ext.clone(),
            is_raw: false,
            has_pair: photo.has_pair,
            pair_id: photo.pair_id.clone(),
            date_time_original: photo.datetime.clone(),
            camera_make: String::new(),
            camera_model: String::new(),
            lens_model: String::new(),
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
            suggested_ev: String::new(),
            stars: String::new(),
            rating_source: "manual".into(),
            faces: photo.faces.to_string(),
            analysis_ok: "true".into(),
            analysis_mode: photo.analysis_mode.clone().unwrap_or_default(),
            burst_group: photo.burst.as_ref().map(|b| b.group.to_string()).unwrap_or_default(),
            burst_size: photo.burst.as_ref().map(|b| b.size.to_string()).unwrap_or_default(),
            burst_rank: photo.burst.as_ref().map(|b| b.rank.to_string()).unwrap_or_default(),
            burst_keep: photo
                .burst
                .as_ref()
                .map(|b| b.keep.to_string())
                .unwrap_or_default(),
            burst_pose: photo
                .burst
                .as_ref()
                .map(|b| b.pose_cluster.to_string())
                .unwrap_or_default(),
        };
        let jpg_path = match guard.photos.iter().find(|x| x.pair_id == photo.pair_id && !x.is_raw) {
            Some(jpg) => state.root.join(&jpg.path),
            None => return (StatusCode::BAD_REQUEST, "该照片没有可评分 JPG").into_response(),
        };
        let sidecar_entries = guard.photos.iter().filter(|x| x.pair_id == photo.pair_id)
            .map(|x| crate::scan::PhotoEntry {
                path: state.root.join(&x.path).display().to_string(),
                filename: x.filename.clone(),
                ..entry.clone()
            }).collect::<Vec<_>>();
        (px, s.total, s.suggested_ev, photo.pair_id.clone(), jpg_path, sidecar_entries)
    };
    let root = state.root.clone();
    let stars = p.stars;
    let warnings = match tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
        crate::decision::set(&root, &jpg_path, stars)?;
        let mut warnings = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for entry in &sidecar_entries {
            let sidecar = Path::new(&entry.path)
                .with_file_name(format!("{}.xmp", crate::scan::stem_raw_of(&entry.filename)));
            if !seen.insert(sidecar) { continue; }
            match crate::output::xmp::write_sidecar(&root, entry, &scores, total, stars, suggested_ev) {
                Ok(true) => {},
                Ok(false) => warnings.push(format!("{} 已有其他软件的侧车，未覆盖", entry.path)),
                Err(err) => warnings.push(format!("{} 侧车写入失败: {err:#}", entry.path)),
            }
        }
        Ok(warnings)
    }).await {
        Ok(Ok(warnings)) => warnings,
        Ok(Err(err)) => return internal_error(err),
        Err(err) => return internal_error(err.into()),
    };
    let mut guard = state.snapshot.write().unwrap();
    for photo in guard.photos.iter_mut().filter(|x| x.pair_id == pair_id) {
        photo.stars = Some(p.stars);
        photo.rating_source = Some("manual".into());
    }
    json_response(serde_json::json!({ "ok": true, "stars": p.stars, "warnings": warnings }))
}

/// GET /api/config：当前可编辑配置值（文件不存在时为内置默认）
async fn config_get(State(state): State<Arc<AppState>>) -> Response {
    let root = state.root.clone();
    let config_path = state.config_path.clone();
    let explicit = state.config_explicit;
    match tokio::task::spawn_blocking(move || {
        load_review_config(&root, explicit.then_some(config_path.as_path()))
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
    let _write_guard = state.write_lock.lock().await;
    let Some(_mutation) = state.job.begin_mutation() else {
        return (StatusCode::CONFLICT, "评分任务进行中或服务正在关闭，暂不能保存配置").into_response();
    };
    let root = state.root.clone();
    let config_path = state.config_path.clone();
    match tokio::task::spawn_blocking(move || {
        config_edit::save_in(&root, &config_path, &values)
    }).await {
        Ok(Ok(())) => {
            let note = "已保存；下一次 UI 跑批生效";
            json_response(serde_json::json!({ "ok": true, "note": note }))
        }
        Ok(Err(err)) => (StatusCode::BAD_REQUEST, format!("{err:#}")).into_response(),
        Err(err) => internal_error(err.into()),
    }
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
    match FileRef::inside(&state.root, &state.root.join(p)).and_then(|file| file.open_read()) {
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
            root: PathBuf::new(), cache_path: PathBuf::new(), config_path: PathBuf::new(),
            config_explicit: false, burst_overrides: RwLock::new(HashMap::new()),
            burst_overrides_path: PathBuf::new(), scene_feedback: RwLock::new(HashMap::new()),
            scene_feedback_path: PathBuf::new(),
            image_slots: Arc::new(Semaphore::new(2)), thumb_slots: Arc::new(Semaphore::new(1)),
            snapshot: RwLock::new(Snapshot {
                root: String::new(), weights: snapshot::WeightsJson {
                    sharpness: 0.3, exposure: 0.25, noise: 0.15,
                    composition: 0.15, aesthetic: 0.15,
                }, photos: vec![],
            }),
            loading: AtomicBool::new(false), load_error: Mutex::new(None),
            load_progress: Mutex::new(LoadProgress { phase: String::new(), done: 0, total: 0 }),
            include_raw: true,
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
        for path in ["/api/control", "/api/score/run", "/api/rate", "/api/burst/keep",
            "/api/config", "/api/scene-feedback", "/api/ui-settings"] {
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
            &dir.join("missing.sqlite"), &HashMap::new(), &HashMap::new()).unwrap();
        // A second file on disk must not be served until the catalogue is refreshed.
        std::fs::write(dir.join("d/unlisted.jpg"), b"unlisted").unwrap();
        let state = AppState {
            root: dir.clone(),
            cache_path: dir.join("c.sqlite"),
            config_path: dir.join("firstcut.toml"),
            config_explicit: true,
            burst_overrides: RwLock::new(HashMap::new()),
            burst_overrides_path: dir.join(".firstcut").join("burst-overrides.json"),
            scene_feedback: RwLock::new(HashMap::new()),
            scene_feedback_path: scene_feedback::store_path(&dir),
            image_slots: Arc::new(Semaphore::new(2)), thumb_slots: Arc::new(Semaphore::new(1)),
            snapshot: RwLock::new(snapshot),
            job: Arc::new(JobState::new()),
            loading: AtomicBool::new(false), load_error: Mutex::new(None),
            load_progress: Mutex::new(LoadProgress { phase: String::new(), done: 0, total: 0 }),
            include_raw: true,
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
        std::fs::create_dir_all(&dir).unwrap();
        let override_path = dir.join(".firstcut").join("burst-overrides.json");
        let photo = snapshot::PhotoJson {
            path: "scene/a.jpg".into(), filename: "a.jpg".into(), ext: "jpg".into(),
            is_raw: false, has_pair: false, pair_id: "a".into(), datetime: String::new(),
            iso: String::new(), f_number: String::new(), shutter: String::new(), focal: String::new(),
            scores: None, stars: None, rating_source: None, analysis_mode: None, faces: 0,
            scene_hint: snapshot::SceneHintJson { id: "unknown", label: "未识别", reason: String::new() },
            scene_feedback: None,
            burst: Some(snapshot::BurstJson { group: 7, size: 1, rank: 1, keep: true,
                suggested_keep: true, manual_keep: None, pose_cluster: 1 }),
        };
        let state = Arc::new(AppState {
            root: dir.clone(), cache_path: dir.join("cache.sqlite"), config_path: dir.join("config.toml"),
            config_explicit: false, burst_overrides: RwLock::new(HashMap::new()),
            burst_overrides_path: override_path.clone(),
            scene_feedback: RwLock::new(HashMap::new()), scene_feedback_path: scene_feedback::store_path(&dir),
            image_slots: Arc::new(Semaphore::new(2)), thumb_slots: Arc::new(Semaphore::new(1)),
            snapshot: RwLock::new(Snapshot { root: dir.display().to_string(),
                weights: snapshot::WeightsJson { sharpness: 0.3, exposure: 0.25, noise: 0.15,
                    composition: 0.15, aesthetic: 0.15 }, photos: vec![photo] }),
            job: Arc::new(JobState::new()),
            loading: AtomicBool::new(false), load_error: Mutex::new(None),
            load_progress: Mutex::new(LoadProgress { phase: String::new(), done: 0, total: 0 }),
            include_raw: true,
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            control: None,
        });
        let response = burst_keep(State(Arc::clone(&state)), axum::Json(BurstKeepParams {
            p: "scene/a.jpg".into(), keep: Some(false),
        })).await;
        assert_eq!(response.status(), StatusCode::OK);
        let saved: HashMap<String, bool> = serde_json::from_slice(&std::fs::read(&override_path).unwrap()).unwrap();
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
        assert!(!state.burst_overrides.read().unwrap().contains_key("scene/a.jpg"));
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
            is_raw: false, has_pair: false, pair_id: "a".into(), datetime: String::new(),
            iso: String::new(), f_number: String::new(), shutter: String::new(), focal: String::new(),
            scores: Some(snapshot::ScoresJson { sharpness: 40.0, exposure: 60.0, noise: 80.0,
                composition: 50.0, aesthetic: 70.0, total: 57.5, suggested_ev: None }),
            stars: Some(3), rating_source: None, analysis_mode: Some("ai".into()), faces: 1,
            burst: None, scene_hint: snapshot::SceneHintJson { id: "portrait", label: "人像候选",
                reason: "检测到 1 张人脸".into() }, scene_feedback: None,
        };
        let state = Arc::new(AppState {
            root: dir.clone(), cache_path: dir.join("cache.sqlite"), config_path: dir.join("config.toml"),
            config_explicit: false, burst_overrides: RwLock::new(HashMap::new()),
            burst_overrides_path: dir.join(".firstcut").join("burst-overrides.json"),
            scene_feedback: RwLock::new(HashMap::new()), scene_feedback_path: feedback_path.clone(),
            image_slots: Arc::new(Semaphore::new(2)), thumb_slots: Arc::new(Semaphore::new(1)),
            snapshot: RwLock::new(Snapshot { root: dir.display().to_string(),
                weights: snapshot::WeightsJson { sharpness: 0.3, exposure: 0.25, noise: 0.15,
                    composition: 0.15, aesthetic: 0.15 }, photos: vec![photo] }),
            job: Arc::new(JobState::new()),
            loading: AtomicBool::new(false), load_error: Mutex::new(None),
            load_progress: Mutex::new(LoadProgress { phase: String::new(), done: 0, total: 0 }),
            include_raw: true,
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
        std::fs::write(dir.join("a.jpg"), b"jpg").unwrap();
        let photo = snapshot::PhotoJson {
            path: "a.jpg".into(), filename: "a.jpg".into(), ext: "jpg".into(),
            is_raw: false, has_pair: false, pair_id: "a".into(), datetime: String::new(),
            iso: String::new(), f_number: String::new(), shutter: String::new(), focal: String::new(),
            scores: Some(snapshot::ScoresJson { sharpness: 70.0, exposure: 75.0, noise: 80.0,
                composition: 60.0, aesthetic: 65.0, total: 70.5, suggested_ev: Some(0.33) }),
            stars: Some(3), rating_source: Some("algorithm".into()),
            analysis_mode: Some("pixel".into()), faces: 0, burst: None,
            scene_hint: snapshot::SceneHintJson { id: "unknown", label: "未识别", reason: String::new() },
            scene_feedback: None,
        };
        let state = Arc::new(AppState {
            root: dir.clone(), cache_path: dir.join(".firstcut/cache.sqlite"),
            config_path: dir.join(".firstcut/config.toml"),
            config_explicit: false, burst_overrides: RwLock::new(HashMap::new()),
            burst_overrides_path: dir.join(".firstcut/burst-overrides.json"),
            scene_feedback: RwLock::new(HashMap::new()), scene_feedback_path: scene_feedback::store_path(&dir),
            image_slots: Arc::new(Semaphore::new(2)), thumb_slots: Arc::new(Semaphore::new(1)),
            snapshot: RwLock::new(Snapshot { root: dir.display().to_string(),
                weights: snapshot::WeightsJson { sharpness: 0.3, exposure: 0.25, noise: 0.15,
                    composition: 0.15, aesthetic: 0.15 }, photos: vec![photo] }),
            job: Arc::new(JobState::new()),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            loading: AtomicBool::new(false), load_error: Mutex::new(None),
            load_progress: Mutex::new(LoadProgress { phase: String::new(), done: 0, total: 0 }),
            include_raw: true,
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
        let sidecar = std::fs::read_to_string(dir.join("a.xmp")).unwrap();
        assert!(sidecar.contains(&format!("<xmp:Rating>{final_star}</xmp:Rating>")), "XMP 与快照星级必须一致");
        assert!(sidecar.contains("crs:Exposure2012=\"+0.33\""));
    }
}
