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

use anyhow::{bail, Result};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// 模型目录（相对当前工作目录）
pub const MODELS_DIR: &str = "models";

/// ort 的错误不含 Send+Sync（内含裸指针），包装成 anyhow::Error
pub fn ort_err(e: impl std::fmt::Debug) -> anyhow::Error {
    anyhow::Error::msg(format!("onnxruntime: {e:?}"))
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
    for f in [
        "clipiqa_model.onnx",
        "clipiqa_model.onnx.data",
        "scrfd_10g_bnkps.onnx",
        "yolov8n_pose.onnx",
    ] {
        if !Path::new(MODELS_DIR).join(f).exists() {
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
