//! 本地 Web 复核界面（M-UI1，只读）
//!
//! axum 服务 + 内嵌 vanilla JS 前端（无 npm 工具链，单 exe 交付）。
//! 只读：除缩略图缓存目录 `.firstcut/thumbs/` 外不写任何文件；
//! 所有取图请求的路径参数强制限制在扫描根目录内。

pub mod snapshot;
pub mod thumb;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{AppendHeaders, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;

use crate::config::ScoreConfig;
use snapshot::Snapshot;

/// 内嵌前端（单文件 HTML，内联 CSS/JS）
const INDEX_HTML: &[u8] = include_bytes!("assets/index.html");

pub struct AppState {
    root: PathBuf,
    snapshot: Snapshot,
}

/// 构建快照并启动服务（阻塞直到服务退出/出错）。
///
/// 绑定成功后（可选）自动打开浏览器。
pub fn serve(root: &Path, cfg: &ScoreConfig, cache_path: &Path, port: u16, open_browser: bool) -> Result<()> {
    eprintln!("[review] 正在构建快照（扫描 + 缓存）……");
    let snapshot = snapshot::build_snapshot(root, cfg, cache_path)?;
    let scored = snapshot.photos.iter().filter(|p| p.scores.is_some()).count();
    eprintln!(
        "[review] 快照就绪：{} 张照片（已评分 {}，未评分 {}）",
        snapshot.photos.len(),
        scored,
        snapshot.photos.len() - scored
    );

    let state = Arc::new(AppState { root: root.to_path_buf(), snapshot });
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let app = Router::new()
            .route("/", get(index))
            .route("/api/photos", get(photos))
            .route("/thumb", get(thumb))
            .route("/image", get(image))
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
    (AppendHeaders([(header::CONTENT_TYPE, "text/html; charset=utf-8")]), INDEX_HTML).into_response()
}

async fn photos(State(state): State<Arc<AppState>>) -> Response {
    match serde_json::to_vec(&state.snapshot) {
        Ok(bytes) => (
            AppendHeaders([(header::CONTENT_TYPE, "application/json")]),
            bytes,
        )
            .into_response(),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")).into_response(),
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
            snapshot: Snapshot { root: dir.display().to_string(), photos: vec![] },
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
