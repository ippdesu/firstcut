//! 本地 Web 复核与操作台（M-UI1 只读复核 + M-UI2 跑批/星级/配置）
//!
//! axum 服务 + 内嵌 vanilla JS 前端（无 npm 工具链，单 exe 交付）。
//! 写入面（全部明示）：缩略图缓存 `.firstcut/thumbs/`、XMP 侧车（跑批 --xmp
//! 与 UI 内改星，均沿用他人侧车保护）、配置文件（UI 编辑）、CSV 报告（跑批）。
//! 所有取图请求的路径参数强制限制在扫描根目录内。

pub mod config_edit;
pub mod job;
pub mod snapshot;
pub mod thumb;

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::Result;
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{AppendHeaders, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;

use crate::config::ScoreConfig;
use crate::review::job::{JobState, JobStatus};
use snapshot::Snapshot;

/// 内嵌前端（单文件 HTML，内联 CSS/JS）
const INDEX_HTML: &[u8] = include_bytes!("assets/index.html");

pub struct AppState {
    root: PathBuf,
    cache_path: PathBuf,
    /// UI 配置编辑的目标文件（--config 指定；缺省 firstcut.toml）
    config_path: PathBuf,
    /// 当前快照；跑批完成后热替换
    snapshot: RwLock<Snapshot>,
    /// 全局单评分任务
    job: Arc<JobState>,
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
    eprintln!("[review] 正在构建快照（扫描 + 缓存）……");
    let snapshot = snapshot::build_snapshot(root, cfg, cache_path)?;
    let scored = snapshot.photos.iter().filter(|p| p.scores.is_some()).count();
    eprintln!(
        "[review] 快照就绪：{} 张照片（已评分 {}，未评分 {}）",
        snapshot.photos.len(),
        scored,
        snapshot.photos.len() - scored
    );

    let config_path =
        config_path.map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("firstcut.toml"));
    let state = Arc::new(AppState {
        root: root.to_path_buf(),
        cache_path: cache_path.to_path_buf(),
        config_path,
        snapshot: RwLock::new(snapshot),
        job: Arc::new(JobState::new()),
    });
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(async move {
        let app = Router::new()
            .route("/", get(index))
            .route("/api/photos", get(photos))
            .route("/thumb", get(thumb))
            .route("/image", get(image))
            .route("/api/job", get(job_status))
            .route("/api/score/run", post(score_run))
            .route("/api/rate", post(rate))
            .route("/api/config", get(config_get).post(config_save))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
        let url = format!("http://127.0.0.1:{port}/");
        eprintln!("[review] 就绪: {url}（Ctrl+C 退出）");
        if open_browser {
            // Windows：start 的第一个引号参数是窗口标题，占位空串
            let _ = std::process::Command::new("cmd").args(["/C", "start", "", &url]).spawn();
        }
        axum::serve(listener, app).await
    })?;
    Ok(())
}

async fn index() -> Response {
    (
        AppendHeaders([(header::CONTENT_TYPE, "text/html; charset=utf-8")]),
        INDEX_HTML,
    )
        .into_response()
}

async fn photos(State(state): State<Arc<AppState>>) -> Response {
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
async fn score_run(State(state): State<Arc<AppState>>, body: Option<axum::Json<ScoreRunParams>>) -> Response {
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
                // --config 未指定（firstcut.toml 不存在）时用内置默认
                if !config_path.exists() {
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
            "[score] 使用内置默认配置（未找到 firstcut.toml）".into()
        });

        let opts = crate::score::ScoreJobOptions {
            output_csv: PathBuf::from("report.csv"),
            cache_path: cache_path.clone(),
            no_cache: false,
            no_ai,
            xmp,
            gpu: false,
            keep_override: None,
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
                match snapshot::build_snapshot(&root, &cfg, &cache_path) {
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

/// POST /api/rate：UI 内改星——只改 `xmp:Rating`，firstcut 子分字段保留；
/// 他人侧车（无 firstcut 命名空间）不覆盖（沿用 write_sidecar 保护）
async fn rate(State(state): State<Arc<AppState>>, axum::Json(p): axum::Json<RateParams>) -> Response {
    if !(1..=5).contains(&p.stars) {
        return (StatusCode::BAD_REQUEST, "星级必须是 1~5").into_response();
    }
    if state.job.is_running() {
        return (StatusCode::CONFLICT, "评分任务进行中，暂不能改星").into_response();
    }
    // 从当前快照取该照片的分数（未评分照片没有可写子分，拒绝）
    let (scores, total, entry) = {
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
            pair_id: String::new(),
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
            stars: String::new(),
            faces: photo.faces.to_string(),
            analysis_ok: "true".into(),
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
        (px, s.total, entry)
    };

    match crate::output::xmp::write_sidecar(&entry, &scores, total, p.stars) {
        Ok(true) => {
            // 内存快照同步（同 stem 的 JPG/ARW 行共享星级）
            {
                let mut guard = state.snapshot.write().unwrap();
                let stem = crate::scan::stem_of(&p.p);
                for photo in guard.photos.iter_mut() {
                    if crate::scan::stem_of(&photo.path) == stem {
                        photo.stars = Some(p.stars);
                    }
                }
            }
            json_response(serde_json::json!({ "ok": true, "stars": p.stars }))
        }
        Ok(false) => (
            StatusCode::CONFLICT,
            "该照片已有其他软件写的侧车（无 firstcut 标记），未覆盖",
        )
            .into_response(),
        Err(err) => internal_error(err),
    }
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
    axum::Json(values): axum::Json<config_edit::ConfigValues>,
) -> Response {
    match config_edit::save(&state.config_path, &values) {
        Ok(()) => {
            let note = if state.config_path.exists() {
                "已保存；下一次跑批/评分即生效"
            } else {
                "已保存；下次启动 review 时用 --config 指定该文件生效"
            };
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

#[cfg(test)]
mod tests {
    use super::*;

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
            snapshot: RwLock::new(Snapshot { root: dir.display().to_string(), photos: vec![] }),
            job: Arc::new(JobState::new()),
        };
        let ok = resolve_jpeg(&state, "d/a.jpg");
        assert!(ok.is_ok(), "jpg 应放行");
        let raw = resolve_jpeg(&state, "d/b.ARW");
        assert!(raw.is_err(), "非 jpg 应 403");
        let missing = resolve_jpeg(&state, "d/none.jpg");
        assert!(missing.is_err(), "不存在的路径应 403（canonicalize 失败）");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
