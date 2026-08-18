//! AI 推理模块（M3）：MUSIQ 美学评分 + YuNet 人脸检测
//!
//! 模型文件位于 models/（已 gitignore）：
//! - musiq_model.onnx + musiq_model.onnx.data（86Cao/IQA-ONNX-Models，hf-mirror 下载）
//! - face_detection_yunet_2023mar.onnx（opencv_zoo，Git LFS media 下载）

pub mod facedetect;
pub mod musiq;

use anyhow::{bail, Result};
use std::path::Path;

/// 模型目录（相对当前工作目录）
pub const MODELS_DIR: &str = "models";

/// ort 的错误不含 Send+Sync（内含裸指针），包装成 anyhow::Error
pub fn ort_err(e: impl std::fmt::Debug) -> anyhow::Error {
    anyhow::Error::msg(format!("onnxruntime: {e:?}"))
}

/// 校验模型文件是否存在；缺失时给出明确指引
pub fn ensure_models() -> Result<()> {
    for f in [
        "musiq_model.onnx",
        "musiq_model.onnx.data",
        "face_detection_yunet_2023mar.onnx",
    ] {
        if !Path::new(MODELS_DIR).join(f).exists() {
            bail!(
                "缺少模型文件 models/{f}\n\
                 请先下载（见 DESIGN.md 9.1 节）：\n\
                 - MUSIQ: hf-mirror.com/86Cao/IQA-ONNX-Models\n\
                 - YuNet: opencv_zoo face_detection_yunet_2023mar.onnx"
            );
        }
    }
    Ok(())
}
