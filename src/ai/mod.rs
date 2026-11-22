//! AI 推理模块（M3/M5）：CLIPIQA 美学评分 + SCRFD 人脸检测 + YOLOv8-pose 姿态检测
//!
//! 三个模型均走 `ort` (onnxruntime-rs)，静态链接自包含，免 DLL：
//! - **CLIPIQA+**（`models/clipiqa_model.onnx` + `.onnx.data`）：
//!   [86Cao/IQA-ONNX-Models](https://huggingface.co/86Cao/IQA-ONNX-Models)，
//!   224×224 输入、CLIP 归一化、sigmoid 输出 ×100 → 0-100 美学分。
//! - **SCRFD 10g**（`models/scrfd_10g_bnkps.onnx`）：
//!   [RuteNL/SCRFD-face-detection-ONNX](https://huggingface.co/RuteNL/SCRFD-face-detection-ONNX)，
//!   640×640 输入、3 尺度检测 + 贪心 NMS，小脸/侧脸检出优于 YuNet。
//! - **YOLOv8n-pose**（`models/yolov8n_pose.onnx`）：
//!   [Xenova/yolov8n-pose](https://huggingface.co/Xenova/yolov8n-pose)，
//!   640×640 输入、SCRFD 漏检时定位人体框与头部关键点，供"主体区域锐度"评估。

pub mod facedetect;
pub mod iqa;
pub mod pose;

use anyhow::{bail, Context, Result};
use ort::session::{builder::SessionBuilder, Session};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::sync::atomic::{AtomicUsize, Ordering};

/// 模型目录名。
pub const MODELS_DIR: &str = "models";

// LFS SHA-256 values at the exact upstream revisions listed in README.
pub(crate) const MODEL_FILES: [(&str, u64, &str); 4] = [
    ("clipiqa_model.onnx", 409_923, "4f959db3c321a652ef45915aeade0eb05284a8145cc933dd9feaae4e633cac37"),
    ("clipiqa_model.onnx.data", 152_764_416, "8c6b5a074405e85cda90155975588a3c4bc500d858ad994423380115610c19b6"),
    ("scrfd_10g_bnkps.onnx", 16_923_827, "5838f7fe053675b1c7a08b633df49e7af5495cee0493c7dcf6697200b85b5b91"),
    ("yolov8n_pose.onnx", 13_484_153, "04f6d2416266f2aba6c5ba8b26de33ed9eba3279f972ac02e33f9f1366547586"),
];

fn model_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("FIRSTCUT_MODEL_DIR") {
        return path.into();
    }
    PathBuf::from(MODELS_DIR)
}

struct VerifiedModel {
    bytes: Vec<u8>,
    external: Option<&'static [u8]>,
}

static CLIP_DATA: OnceLock<Box<[u8]>> = OnceLock::new();
static CLIP_LOAD: Mutex<()> = Mutex::new(());
// ORT keeps a reference to the external initializer for each session. There
// is exactly one accepted data file, so all sessions share these bytes.
fn clip_data(dir: &crate::storage::Folder) -> Result<&'static [u8]> {
    let _load = CLIP_LOAD.lock()
        .map_err(|_| anyhow::anyhow!("无法共享模型权重"))?;
    if CLIP_DATA.get().is_none() {
        let bytes = verify_model_file(dir, MODEL_FILES[1])?.into_boxed_slice();
        let _ = CLIP_DATA.set(bytes);
    }
    Ok(CLIP_DATA.get().expect("initializer set under lock").as_ref())
}

impl VerifiedModel {
    fn commit(&self, mut builder: SessionBuilder) -> Result<Session> {
        if let Some(data) = self.external {
            builder = builder.with_external_initializer_file_in_memory(
                "clipiqa_model.onnx.data", Cow::Borrowed(data)).map_err(ort_err)?;
        }
        builder.commit_from_memory(&self.bytes).map_err(ort_err)
    }
}

fn verify_model_file(dir: &crate::storage::Folder, spec: (&str, u64, &str)) -> Result<Vec<u8>> {
    let (name, size, expected) = spec;
    let content = dir.file(name)?.read(size)?
        .with_context(|| format!("缺少已支持的模型文件 {name}"))?;
    anyhow::ensure!(content.len() as u64 == size && format!("{:x}", Sha256::digest(&content)) == expected,
        "模型 {name} 校验不匹配；请从 README 列出的固定版本重新下载");
    Ok(content)
}

fn load_verified_model(model_file: &str) -> Result<VerifiedModel> {
    let spec = MODEL_FILES.iter().copied().find(|spec| spec.0 == model_file)
        .with_context(|| format!("未知模型文件 {model_file}"))?;
    let canonical_dir = crate::review::normal_photo_root(&model_dir())
        .context("找不到已支持的模型目录")?;
    let dir = crate::storage::Folder::open(&canonical_dir).context("找不到已支持的模型目录")?;
    let bytes = verify_model_file(&dir, spec)?;
    // The supported CLIPIQA graph and its data are verified separately. Feed
    // the verified bytes directly to ORT so it cannot reopen a changed path.
    let external = if model_file == "clipiqa_model.onnx" {
        Some(clip_data(&dir)?)
    } else { None };
    Ok(VerifiedModel { bytes, external })
}

/// ort 的错误不含 Send+Sync（内含裸指针），包装成 anyhow::Error
pub fn ort_err(e: impl std::fmt::Debug) -> anyhow::Error {
    anyhow::Error::msg(format!("onnxruntime: {e:?}"))
}

/// 统一的 session 构建：优化级别 / intra 线程数 / 实验性 DirectML。
///
/// `use_gpu` 只有在 `--features gpu` 构建下才生效；DirectML 注册失败
/// （驱动/系统不支持）时回落 CPU 并不视为致命错误。
pub fn build_session(model_file: &str, intra: usize, use_gpu: bool) -> Result<Session> {
    let model = load_verified_model(model_file)?;
    #[cfg(feature = "gpu")]
    if use_gpu {
        use ort::ep::DirectML;
        // with_execution_providers 消费 builder，失败时走下方 CPU 路径重建
        match Session::builder()
            .map_err(ort_err)?
            .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level3)
            .map_err(ort_err)?
            .with_intra_threads(intra)
            .map_err(ort_err)?
            .with_execution_providers([DirectML::default().build()])
        {
            Ok(b) => {
                eprintln!("[ai] DirectML GPU 推理已启用（实验性）");
                return model.commit(b);
            }
            Err(e) => {
                eprintln!("[ai] 警告: DirectML 注册失败，回落 CPU: {e:?}");
            }
        }
    }
    #[cfg(not(feature = "gpu"))]
    let _ = use_gpu;

    let builder = Session::builder()
        .map_err(ort_err)?
        .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level3)
        .map_err(ort_err)?
        .with_intra_threads(intra)
        .map_err(ort_err)?;
    model.commit(builder)
}

/// 轻量 session 池：onnxruntime 的 run 需要 &mut self，
/// 多 session 轮询分配实现并行推理（每 session 的 intra-op 线程数需小于核数）。
pub struct SessionPool<T> {
    sessions: Vec<Mutex<T>>,
    next: AtomicUsize,
}

impl<T> SessionPool<T> {
    pub fn new(sessions: Vec<T>) -> Self {
        SessionPool {
            sessions: sessions.into_iter().map(Mutex::new).collect(),
            next: AtomicUsize::new(0),
        }
    }

    /// 轮询获取一个 session（并行安全）
    pub fn acquire(&self) -> std::sync::MutexGuard<'_, T> {
        let i = self.next.fetch_add(1, Ordering::Relaxed) % self.sessions.len();
        self.sessions[i].lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }
}

/// 校验模型文件是否存在；缺失时给出明确指引
pub fn ensure_models() -> Result<()> {
    let dir = model_dir();
    for (f, _, _) in MODEL_FILES {
        if !dir.join(f).is_file() {
            bail!(
                "缺少模型文件 models/{f}\n\
                 请先下载（模型列表见 README.md）：\n\
                 - CLIPIQA+: hf-mirror.com/86Cao/IQA-ONNX-Models\n\
                 - SCRFD:    hf-mirror.com/RuteNL/SCRFD-face-detection-ONNX\n\
                 - YOLOv8n-pose: hf-mirror.com/Xenova/yolov8n-pose"
            );
        }
    }
    Ok(())
}

pub(crate) fn model_path(name: &str) -> PathBuf {
    model_dir().join(name)
}
