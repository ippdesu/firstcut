//! 本地 Web 复核与操作台（M-UI1 只读复核 + M-UI2 跑批/星级/配置）
//!
//! axum 服务 + 内嵌 vanilla JS 前端（无 npm 工具链，单 exe 交付）。
//! 写入面（全部明示）：缩略图缓存 `.firstcut/thumbs/`、XMP 侧车（跑批 --xmp
//! 与 UI 内改星，均沿用他人侧车保护）、配置文件（UI 编辑）、CSV 报告（跑批）。
//! 所有取图请求的路径参数强制限制在扫描根目录内。

pub mod config_edit;
pub mod job;
pub mod scene_feedback;
pub mod snapshot;
pub mod thumb;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::Result;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{AppendHeaders, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::config::ScoreConfig;
use crate::review::job::{JobState, JobStatus};
use scene_feedback::SceneFeedbackRecord;
use snapshot::Snapshot;

/// 内嵌前端（单文件 HTML，内联 CSS/JS）
const INDEX_HTML: &[u8] = include_bytes!("assets/index.html");

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

#[derive(Deserialize, Serialize)]
struct UiPreferences { include_raw: bool }

fn load_ui_preferences(root: &Path) -> bool {
    std::fs::read(root.join(".firstcut/ui-preferences.json"))
        .ok().and_then(|bytes| serde_json::from_slice::<UiPreferences>(&bytes).ok())
        .map(|prefs| prefs.include_raw).unwrap_or(true)
}

fn save_ui_preferences(root: &Path, include_raw: bool) -> Result<()> {
    std::fs::write(root.join(".firstcut/ui-preferences.json"),
        serde_json::to_vec_pretty(&UiPreferences { include_raw })?)?;
    Ok(())
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
    port: u16,
    /// 当前快照；跑批完成后热替换
    snapshot: RwLock<Snapshot>,
    loading: AtomicBool,
    load_error: Mutex<Option<String>>,
    load_progress: Mutex<LoadProgress>,
    include_raw: bool,
    /// 全局单评分任务
    job: Arc<JobState>,
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
    serve_once(root, cfg, cache_path, port, open_browser, config_path, false, true)?;
    Ok(())
}

/// Windows 双击启动器：在同一进程中重启服务，退出时正常释放端口。
pub fn serve_ui(root: &Path, pick_root: impl Fn() -> Option<PathBuf>) -> Result<()> {
    let mut root = normal_photo_root(root)?;
    let mut open_browser = true;
    loop {
        let state_dir = root.join(".firstcut");
        std::fs::create_dir_all(&state_dir)?;
        let cache_path = state_dir.join("cache.sqlite");
        let config_path = root.join(".firstcut").join("config.toml");
        let config = if config_path.exists() {
            crate::config::load_config(&config_path)?
        } else {
            ScoreConfig::default()
        };
        let include_raw = load_ui_preferences(&root);
        match serve_once(&root, &config, &cache_path, 8787, open_browser, None, true, include_raw)? {
            Some(UiAction::Restart) => open_browser = false,
            Some(UiAction::Choose) => {
                if let Some(next) = pick_root() {
                    root = normal_photo_root(&next)?;
                }
                open_browser = false;
            }
            Some(UiAction::Rawon) => { save_ui_preferences(&root, true)?; open_browser = false; }
            Some(UiAction::Rawoff) => { save_ui_preferences(&root, false)?; open_browser = false; }
            _ => return Ok(()),
        }
    }
}

fn serve_once(
    root: &Path,
    cfg: &ScoreConfig,
    cache_path: &Path,
    port: u16,
    open_browser: bool,
    config_path: Option<&Path>,
    ui_control: bool,
    include_raw: bool,
) -> Result<Option<UiAction>> {
    let burst_overrides_path = root.join(".firstcut").join("burst-overrides.json");
    let burst_overrides = load_burst_overrides(&burst_overrides_path);
    let scene_feedback_path = scene_feedback::store_path(root);
    let feedback = scene_feedback::load_latest(&scene_feedback_path)?;
    let initial_snapshot = if ui_control {
        Snapshot::empty(root, cfg)
    } else {
        build_review_snapshot(root, cfg, cache_path, &burst_overrides, &feedback,
            include_raw, false, &|_, _, _| {})?
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
        port,
        snapshot: RwLock::new(initial_snapshot),
        loading: AtomicBool::new(ui_control),
        load_error: Mutex::new(None),
        load_progress: Mutex::new(LoadProgress {
            phase: "准备读取照片".into(), done: 0, total: 0,
        }),
        include_raw,
        job: Arc::new(JobState::new()),
        control: control.clone(),
    });
    let load_state = Arc::clone(&state);
    let load_root = root.to_path_buf();
    let load_cfg = cfg.clone();
    let load_cache = cache_path.to_path_buf();
    let shutdown_control = control.clone();
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(async move {
        let app = Router::new()
            .route("/", get(index))
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
            .route("/api/control", get(control_status).post(control_post))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
        if ui_control {
            std::thread::spawn(move || {
                let on_progress = |phase: &str, done: usize, total: usize| {
                    *load_state.load_progress.lock().unwrap() = LoadProgress {
                        phase: phase.into(), done, total,
                    };
                };
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    build_review_snapshot(&load_root, &load_cfg, &load_cache,
                        &load_overrides, &load_feedback, include_raw, true, &on_progress)
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
        let url = format!("http://127.0.0.1:{port}/");
        if ui_control {
            quiet_log(format!("[review] 就绪: {url}（页面右上角退出）"));
        } else {
            quiet_log(format!("[review] 就绪: {url}（Ctrl+C 退出）"));
        }
        if open_browser {
            // Windows：start 的第一个引号参数是窗口标题，占位空串
            let _ = std::process::Command::new("cmd").args(["/C", "start", "", &url]).spawn();
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
    match resolve_jpeg(&state, &q.p) {
        Ok(full) => match thumb::get_or_create(&state.root, &q.p, &full) {
            Ok(Some(bytes)) => jpeg_response(bytes),
            Ok(None) => StatusCode::NOT_FOUND.into_response(),
            Err(err) => internal_error(err),
        },
        Err(resp) => resp,
    }
}

/// 原图直读（浏览器原生解码，1:1 预览）
async fn image(State(state): State<Arc<AppState>>, Query(q): Query<PhotoParam>) -> Response {
    match resolve_jpeg(&state, &q.p) {
        Ok(full) => match std::fs::read(&full) {
            Ok(bytes) => jpeg_response(bytes),
            Err(err) => internal_error(err.into()),
        },
        Err(resp) => resp,
    }
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
struct ControlRequest {
    action: UiAction,
}

async fn control_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Json(request): axum::Json<ControlRequest>,
) -> Response {
    if !valid_local_host(&headers, state.port) || !valid_control_origin(&headers, state.port) {
        return StatusCode::FORBIDDEN.into_response();
    }
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
async fn score_run(State(state): State<Arc<AppState>>, headers: HeaderMap,
    body: Option<axum::Json<ScoreRunParams>>) -> Response {
    if !valid_local_host(&headers, state.port) { return StatusCode::FORBIDDEN.into_response(); }
    if state.loading.load(Ordering::Acquire) {
        return (StatusCode::CONFLICT, "照片仍在加载，请稍后再评分").into_response();
    }
    let params = body.map(|axum::Json(p)| p).unwrap_or(ScoreRunParams { xmp: false, no_ai: false });
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
        job.log("[score] 任务开始");
        // 每次跑批都重新加载配置：UI 里的配置编辑保存后下一次跑批即生效
        let cfg_result = match crate::config::load_config(&config_path) {
            Ok(c) => Ok(c),
            Err(err) => {
                // 未显式指定配置且默认文件不存在时用内置默认
                if !config_path.exists() && !st.config_explicit {
                    Ok(ScoreConfig::default())
                } else {
                    Err(err)
                }
            }
        };
        let cfg = match cfg_result {
            Ok(c) => c,
            Err(err) => {
                job.log(format!("[score] 配置加载失败: {err:#}"));
                job.fail(format!("配置加载失败: {err:#}"));
                return;
            }
        };
        job.log(if config_path.exists() {
            format!("[score] 配置已加载: {}", config_path.display())
        } else {
            "[score] 使用内置默认配置（照片根目录尚无 UI 配置）".into()
        });

        let opts = crate::score::ScoreJobOptions {
            output_csv: root.join(".firstcut").join("report.csv"),
            cache_path: cache_path.clone(),
            no_cache: false,
            no_ai,
            xmp,
            gpu: false,
            keep_override: None,
            include_raw: st.include_raw,
            skip_processed: st.control.is_some(),
        };
        if let Some(parent) = opts.output_csv.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                job.fail(format!("无法创建报告目录: {err}"));
                return;
            }
        }
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
                    &overrides, &feedback, st.include_raw, st.control.is_some(), &|_, _, _| {}) {
                    Ok(snap) => {
                        *st.snapshot.write().unwrap() = snap;
                        job.log("[review] 快照已刷新");
                    }
                    Err(err) => {
                        job.log(format!("[review] 快照刷新失败: {err:#}"));
                    }
                }
                job.finish(format!(
                    "完成：{} 张 JPG（命中 {}，新分析 {}，失败 {}，ARW 无映射 {}）→ {}",
                    summary.total_jpg,
                    summary.hits,
                    summary.misses,
                    summary.failed_jpgs.len(),
                    summary.unmapped_arw,
                    summary.csv_path.display()
                ));
            }
            Err(err) => {
                job.log(format!("[score] 任务失败: {err:#}"));
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
async fn burst_keep(State(state): State<Arc<AppState>>, headers: HeaderMap,
    axum::Json(p): axum::Json<BurstKeepParams>) -> Response {
    if !valid_local_host(&headers, state.port) { return StatusCode::FORBIDDEN.into_response(); }
    if state.job.is_running() {
        return (StatusCode::CONFLICT, "评分任务进行中，暂不能修改连拍标记").into_response();
    }
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

    let mut overrides = state.burst_overrides.write().unwrap();
    let previous = overrides.get(&p.p).copied();
    if let Some(keep) = p.keep { overrides.insert(p.p.clone(), keep); }
    else { overrides.remove(&p.p); }
    let bytes = match serde_json::to_vec_pretty(&*overrides) {
        Ok(bytes) => bytes,
        Err(err) => return internal_error(err.into()),
    };
    if let Some(parent) = state.burst_overrides_path.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            restore_override(&mut overrides, &p.p, previous);
            return internal_error(err.into());
        }
    }
    if let Err(err) = std::fs::write(&state.burst_overrides_path, bytes) {
        restore_override(&mut overrides, &p.p, previous);
        return internal_error(err.into());
    }
    drop(overrides);

    let mut guard = state.snapshot.write().unwrap();
    for photo in guard.photos.iter_mut().filter(|x| x.pair_id == pair_id) {
        if let Some(burst) = &mut photo.burst {
            burst.keep = p.keep.unwrap_or(burst.suggested_keep);
            burst.manual_keep = p.keep;
        }
    }
    json_response(serde_json::json!({"ok": true, "keep": p.keep}))
}

fn restore_override(overrides: &mut HashMap<String, bool>, path: &str, previous: Option<bool>) {
    if let Some(value) = previous { overrides.insert(path.to_string(), value); }
    else { overrides.remove(path); }
}

fn load_burst_overrides(path: &Path) -> HashMap<String, bool> {
    std::fs::read(path).ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

#[derive(Deserialize)]
struct SceneFeedbackParams {
    p: String,
    selected_scene: String,
    #[serde(default)]
    note: String,
}

/// POST /api/scene-feedback：记录场景纠错与当前评分线索，供后续校准。
async fn scene_feedback_save(State(state): State<Arc<AppState>>, headers: HeaderMap,
    axum::Json(p): axum::Json<SceneFeedbackParams>) -> Response {
    if !valid_local_host(&headers, state.port) { return StatusCode::FORBIDDEN.into_response(); }
    if !scene_feedback::valid_scene(&p.selected_scene) {
        return (StatusCode::BAD_REQUEST, "请选择有效的场景").into_response();
    }
    if p.note.chars().count() > 1000 {
        return (StatusCode::BAD_REQUEST, "备注不能超过 1000 字").into_response();
    }
    if state.job.is_running() {
        return (StatusCode::CONFLICT, "评分任务进行中，暂不能记录场景").into_response();
    }
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
    let mut feedback = state.scene_feedback.write().unwrap();
    if let Err(err) = scene_feedback::append(&state.scene_feedback_path, &record) {
        return internal_error(err);
    }
    feedback.insert(record.photo_path.clone(), record.clone());
    drop(feedback);
    let mut guard = state.snapshot.write().unwrap();
    if let Some(photo) = guard.photos.iter_mut().find(|x| x.path == record.photo_path) {
        photo.scene_feedback = Some(record.clone());
    }
    json_response(serde_json::json!({"ok": true, "record": record}))
}

/// POST /api/rate：UI 内改星——只改 `xmp:Rating`，firstcut 子分字段保留；
/// 他人侧车（无 firstcut 命名空间）不覆盖（沿用 write_sidecar 保护）
async fn rate(State(state): State<Arc<AppState>>, headers: HeaderMap,
    axum::Json(p): axum::Json<RateParams>) -> Response {
    if !valid_local_host(&headers, state.port) { return StatusCode::FORBIDDEN.into_response(); }
    if !(1..=5).contains(&p.stars) {
        return (StatusCode::BAD_REQUEST, "星级必须是 1~5").into_response();
    }
    if state.job.is_running() {
        return (StatusCode::CONFLICT, "评分任务进行中，暂不能改星").into_response();
    }
    // 从当前快照取该照片的分数（未评分照片没有可写子分，拒绝）
    let (scores, total, suggested_ev, pair_id, jpg_path, sidecar_entries) = {
        let guard = state.snapshot.read().unwrap();
        let Some(photo) = guard.photos.iter().find(|x| x.path == p.p) else {
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
    if let Err(err) = crate::decision::set(&state.root, &jpg_path, p.stars) {
        return internal_error(err);
    }
    let mut warnings = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for entry in &sidecar_entries {
        let sidecar = Path::new(&entry.path)
            .with_file_name(format!("{}.xmp", crate::scan::stem_raw_of(&entry.filename)));
        if !seen.insert(sidecar) { continue; }
        match crate::output::xmp::write_sidecar(entry, &scores, total, p.stars, suggested_ev) {
            Ok(true) => {},
            Ok(false) => warnings.push(format!("{} 已有其他软件的侧车，未覆盖", entry.path)),
            Err(err) => warnings.push(format!("{} 侧车写入失败: {err:#}", entry.path)),
        }
    }
    let mut guard = state.snapshot.write().unwrap();
    for photo in guard.photos.iter_mut().filter(|x| x.pair_id == pair_id) {
        photo.stars = Some(p.stars);
        photo.rating_source = Some("manual".into());
    }
    json_response(serde_json::json!({ "ok": true, "stars": p.stars, "warnings": warnings }))
}

/// GET /api/config：当前可编辑配置值（文件不存在时为内置默认）
async fn config_get(State(state): State<Arc<AppState>>) -> Response {
    match config_edit::load(&state.config_path) {
        Ok(values) => json_response(serde_json::json!({
            "path": state.config_path.display().to_string(),
            "exists": state.config_path.exists(),
            "values": values,
        })),
        Err(err) => internal_error(err),
    }
}

/// POST /api/config：保存编辑值（toml_edit 保留注释；非法值拒绝写盘）
async fn config_save(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Json(values): axum::Json<config_edit::ConfigValues>,
) -> Response {
    if !valid_local_host(&headers, state.port) { return StatusCode::FORBIDDEN.into_response(); }
    match config_edit::save(&state.config_path, &values) {
        Ok(()) => {
            let note = "已保存；下一次 UI 跑批生效";
            json_response(serde_json::json!({ "ok": true, "note": note }))
        }
        Err(err) => (StatusCode::BAD_REQUEST, format!("{err:#}")).into_response(),
    }
}

fn json_response(v: serde_json::Value) -> Response {
    (
        AppendHeaders([(header::CONTENT_TYPE, "application/json")]),
        serde_json::to_vec(&v).unwrap_or_default(),
    )
        .into_response()
}

/// 路径安全：解析并校验在扫描根目录内，且扩展名为 JPG。
/// Err 分支直接携带要返回的 HTTP 响应（403/404）。
fn resolve_jpeg(state: &AppState, p: &str) -> std::result::Result<PathBuf, Response> {
    match snapshot::resolve_under(&state.root, p) {
        Some(full) => {
            let ext = full
                .extension()
                .map(|e| e.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            if matches!(ext.as_str(), "jpg" | "jpeg") {
                Ok(full)
            } else {
                Err(StatusCode::FORBIDDEN.into_response())
            }
        }
        None => Err(StatusCode::FORBIDDEN.into_response()),
    }
}

fn jpeg_response(bytes: Vec<u8>) -> Response {
    // 缩略图/原图内容只随文件变化；本地服务给 1h 缓存，
    // 灯箱反复开关不再重复传输几十 MB 原图（DSH 第 2 轮附带观察）
    (
        AppendHeaders([
            (header::CONTENT_TYPE, "image/jpeg"),
            (header::CACHE_CONTROL, "private, max-age=3600"),
        ]),
        bytes,
    )
        .into_response()
}

fn internal_error(err: anyhow::Error) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")).into_response()
}

fn valid_local_host(headers: &HeaderMap, port: u16) -> bool {
    let expected_ip = format!("127.0.0.1:{port}");
    let expected_name = format!("localhost:{port}");
    headers.get(header::HOST).and_then(|v| v.to_str().ok())
        .is_some_and(|host| host.eq_ignore_ascii_case(&expected_ip)
            || host.eq_ignore_ascii_case(&expected_name))
}

fn valid_control_origin(headers: &HeaderMap, port: u16) -> bool {
    if !valid_local_host(headers, port) {
        return false;
    }
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let expected = format!("http://{host}");
    headers.get(header::ORIGIN).and_then(|v| v.to_str().ok())
        .is_some_and(|origin| origin.eq_ignore_ascii_case(&expected))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            scene_feedback_path: PathBuf::new(), port: 8787,
            snapshot: RwLock::new(Snapshot {
                root: String::new(), weights: snapshot::WeightsJson {
                    sharpness: 0.3, exposure: 0.25, noise: 0.15,
                    composition: 0.15, aesthetic: 0.15,
                }, photos: vec![],
            }),
            loading: AtomicBool::new(false), load_error: Mutex::new(None),
            load_progress: Mutex::new(LoadProgress { phase: String::new(), done: 0, total: 0 }),
            include_raw: true,
            job: Arc::new(JobState::new()), control: Some(Arc::clone(&control)),
        });
        state.loading.store(true, Ordering::Release);
        assert_eq!(photos(State(Arc::clone(&state))).await.status(), StatusCode::SERVICE_UNAVAILABLE);
        state.loading.store(false, Ordering::Release);
        assert_eq!(photos(State(Arc::clone(&state))).await.status(), StatusCode::OK);
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "127.0.0.1:8787".parse().unwrap());
        headers.insert(header::ORIGIN, "http://untrusted.example".parse().unwrap());
        let response = control_post(State(Arc::clone(&state)), headers.clone(),
            axum::Json(ControlRequest { action: UiAction::Exit })).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(control.action.lock().unwrap().is_none());

        headers.insert(header::ORIGIN, "http://127.0.0.1:8787".parse().unwrap());
        assert!(state.job.begin());
        let response = control_post(State(Arc::clone(&state)), headers.clone(),
            axum::Json(ControlRequest { action: UiAction::Exit })).await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        state.job.finish("完成".into());
        let response = control_post(State(state), headers,
            axum::Json(ControlRequest { action: UiAction::Restart })).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(*control.action.lock().unwrap(), Some(UiAction::Restart));
    }

    #[tokio::test]
    async fn jpeg_whitelist_blocks_arw() {
        // 直接测 resolve_jpeg 的白名单逻辑（不依赖真实服务）
        let dir = std::env::temp_dir().join("firstcut_review_whitelist");
        std::fs::create_dir_all(dir.join("d")).unwrap();
        std::fs::write(dir.join("d/a.jpg"), b"jpeg").unwrap();
        std::fs::write(dir.join("d/b.ARW"), b"raw").unwrap();
        let state = AppState {
            root: dir.clone(),
            cache_path: dir.join("c.sqlite"),
            config_path: dir.join("firstcut.toml"),
            config_explicit: true,
            burst_overrides: RwLock::new(HashMap::new()),
            burst_overrides_path: dir.join(".firstcut").join("burst-overrides.json"),
            scene_feedback: RwLock::new(HashMap::new()),
            scene_feedback_path: scene_feedback::store_path(&dir),
            port: 8787,
            snapshot: RwLock::new(Snapshot { root: dir.display().to_string(),
                weights: snapshot::WeightsJson { sharpness: 0.3, exposure: 0.25, noise: 0.15,
                    composition: 0.15, aesthetic: 0.15 }, photos: vec![] }),
            job: Arc::new(JobState::new()),
            loading: AtomicBool::new(false), load_error: Mutex::new(None),
            load_progress: Mutex::new(LoadProgress { phase: String::new(), done: 0, total: 0 }),
            include_raw: true,
            control: None,
        };
        let ok = resolve_jpeg(&state, "d/a.jpg");
        assert!(ok.is_ok(), "jpg 应放行");
        let raw = resolve_jpeg(&state, "d/b.ARW");
        assert!(raw.is_err(), "非 jpg 应 403");
        let missing = resolve_jpeg(&state, "d/none.jpg");
        assert!(missing.is_err(), "不存在的路径应 403（canonicalize 失败）");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn manual_burst_keep_persists_and_can_restore_suggestion() {
        let dir = std::env::temp_dir().join(format!("firstcut_burst_override_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
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
            port: 8787,
            snapshot: RwLock::new(Snapshot { root: dir.display().to_string(),
                weights: snapshot::WeightsJson { sharpness: 0.3, exposure: 0.25, noise: 0.15,
                    composition: 0.15, aesthetic: 0.15 }, photos: vec![photo] }),
            job: Arc::new(JobState::new()),
            loading: AtomicBool::new(false), load_error: Mutex::new(None),
            load_progress: Mutex::new(LoadProgress { phase: String::new(), done: 0, total: 0 }),
            include_raw: true,
            control: None,
        });
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "127.0.0.1:8787".parse().unwrap());

        let response = burst_keep(State(Arc::clone(&state)), headers.clone(), axum::Json(BurstKeepParams {
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

        let response = burst_keep(State(Arc::clone(&state)), headers, axum::Json(BurstKeepParams {
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
            port: 8787, snapshot: RwLock::new(Snapshot { root: dir.display().to_string(),
                weights: snapshot::WeightsJson { sharpness: 0.3, exposure: 0.25, noise: 0.15,
                    composition: 0.15, aesthetic: 0.15 }, photos: vec![photo] }),
            job: Arc::new(JobState::new()),
            loading: AtomicBool::new(false), load_error: Mutex::new(None),
            load_progress: Mutex::new(LoadProgress { phase: String::new(), done: 0, total: 0 }),
            include_raw: true,
            control: None,
        });
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "127.0.0.1:8787".parse().unwrap());
        let response = scene_feedback_save(State(Arc::clone(&state)), headers.clone(),
            axum::Json(SceneFeedbackParams { p: "a.jpg".into(), selected_scene: "stage".into(),
                note: "舞台灯光".into() })).await;
        assert_eq!(response.status(), StatusCode::OK);
        let saved = scene_feedback::load_latest(&feedback_path).unwrap();
        assert_eq!(saved["a.jpg"].predicted_scene, "portrait");
        assert_eq!(saved["a.jpg"].selected_scene, "stage");
        assert_eq!(saved["a.jpg"].scores.as_ref().unwrap().total, 57.5);
        assert_eq!(state.snapshot.read().unwrap().photos[0].scene_feedback.as_ref().unwrap().note, "舞台灯光");

        let response = scene_feedback_save(State(state), headers,
            axum::Json(SceneFeedbackParams { p: "a.jpg".into(), selected_scene: "invalid".into(),
                note: String::new() })).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(std::fs::read_to_string(&feedback_path).unwrap().lines().count(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }
}
