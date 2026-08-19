//! YuNet 人脸检测（M3）
//!
//! opencv_zoo face_detection_yunet_2023mar.onnx：
//! 输入 1x3x640x640 RGB（[0,1]），输出 12 个原始张量：
//!   cls_8/16/32, obj_8/16/32（[1, N, 1]）、bbox_8/16/32（[1, N, 4]）、kps_8/16/32（[1, N, 10]）
//! 后处理参考 OpenCV 4.x face_detect.cpp：
//!   score = sqrt(clamp(cls) * clamp(obj))，阈值 0.6
//!   cx = (c + bbox[0]) * stride；w = exp(bbox[2]) * stride（无 anchor 尺寸，新式解码）

use anyhow::Result;
use ndarray::Array4;
use ort::session::Session;

/// 检测到的人脸框（坐标已归一化到 0-1）
#[derive(Debug, Clone, Copy)]
pub struct FaceBox {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub score: f32,
}

pub struct YuNet {
    session: Session,
}

/// 模型输入边长（2023mar 版固定 640x640）
const INPUT_SIZE: usize = 640;
/// 三个特征层下采样步长
const STRIDES: [usize; 3] = [8, 16, 32];
/// 置信度阈值（0.6 过紧：实测有人脸照片候选分数 0.59x 被滤，降至 0.5）
const SCORE_THRESHOLD: f32 = 0.5;
/// NMS IoU 阈值
const NMS_IOU: f32 = 0.3;

impl YuNet {
    /// 加载模型；`intra_threads` 为每个 session 的 ORT 内部线程数
    pub fn load(intra_threads: usize) -> Result<Self> {
        let builder = Session::builder().map_err(crate::ai::ort_err)?;
        let session = builder
            .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level3)
            .map_err(crate::ai::ort_err)?
            .with_intra_threads(intra_threads)
            .map_err(crate::ai::ort_err)?
            .commit_from_file(format!(
                "{}/face_detection_yunet_2023mar.onnx",
                crate::ai::MODELS_DIR
            ))
            .map_err(crate::ai::ort_err)?;
        Ok(Self { session })
    }

    /// 检测人脸，返回归一化人脸框列表（已解码 + NMS，阈值 0.6）
    pub fn detect(&mut self, rgb: &[u8], w: u32, h: u32) -> Result<Vec<FaceBox>> {
        self.detect_with_threshold(rgb, w, h, SCORE_THRESHOLD)
    }

    /// 检测人脸（可指定置信度阈值，调试/低阈值场景用）
    pub fn detect_with_threshold(
        &mut self,
        rgb: &[u8],
        w: u32,
        h: u32,
        threshold: f32,
    ) -> Result<Vec<FaceBox>> {
        let img = image::RgbImage::from_raw(w, h, rgb.to_vec())
            .ok_or_else(|| anyhow::anyhow!("RGB 数据尺寸不一致"))?;
        let small = image::DynamicImage::ImageRgb8(img).resize_exact(
            INPUT_SIZE as u32,
            INPUT_SIZE as u32,
            image::imageops::FilterType::Triangle,
        );
        let px = small.into_rgb8();

        let mut arr = Array4::<f32>::zeros((1, 3, INPUT_SIZE, INPUT_SIZE));
        for y in 0..INPUT_SIZE {
            for x in 0..INPUT_SIZE {
                let p = px.get_pixel(x as u32, y as u32);
                // OpenCV 参考实现用默认 blobFromImage（scale=1.0），输入为 0~255 原始值
                arr[[0, 0, y, x]] = p[0] as f32;
                arr[[0, 1, y, x]] = p[1] as f32;
                arr[[0, 2, y, x]] = p[2] as f32;
            }
        }

        let tensor = ort::value::Tensor::from_array(arr).map_err(crate::ai::ort_err)?;
        let input = ort::inputs!["input" => tensor];
        let out = self.session.run(input).map_err(crate::ai::ort_err)?;

        // 解码各尺度候选框（OpenCV 4.x face_detect.cpp 同款逻辑）
        let mut dets: Vec<FaceBox> = Vec::new();
        for (si, &stride) in STRIDES.iter().enumerate() {
            let cols = INPUT_SIZE / stride;
            let rows = INPUT_SIZE / stride;
            let n = cols * rows;

            let (_, cls) = out[si].try_extract_tensor::<f32>().map_err(crate::ai::ort_err)?; // cls_8/16/32
            let (_, obj) = out[si + 3].try_extract_tensor::<f32>().map_err(crate::ai::ort_err)?; // obj_8/16/32
            let (_, bbox) = out[si + 6].try_extract_tensor::<f32>().map_err(crate::ai::ort_err)?; // bbox_8/16/32
            if cls.len() < n || obj.len() < n || bbox.len() < n * 4 {
                anyhow::bail!("YuNet 输出尺寸异常 (stride={stride})");
            }

            for idx in 0..n {
                let cls_score = cls[idx].clamp(0.0, 1.0);
                let obj_score = obj[idx].clamp(0.0, 1.0);
                let score = (cls_score * obj_score).sqrt();
                // 浮点边界保护：0.600 的 f32 表示可能略小于 0.6f32 而误滤
                if score + 1e-5 < threshold {
                    continue;
                }
                let c = (idx % cols) as f32;
                let r = (idx / cols) as f32;
                let b4 = idx * 4;
                let cx = (c + bbox[b4]) * stride as f32;
                let cy = (r + bbox[b4 + 1]) * stride as f32;
                let bw = bbox[b4 + 2].exp() * stride as f32;
                let bh = bbox[b4 + 3].exp() * stride as f32;
                let x1 = (cx - bw / 2.0).clamp(0.0, INPUT_SIZE as f32);
                let y1 = (cy - bh / 2.0).clamp(0.0, INPUT_SIZE as f32);
                let x2 = (cx + bw / 2.0).clamp(0.0, INPUT_SIZE as f32);
                let y2 = (cy + bh / 2.0).clamp(0.0, INPUT_SIZE as f32);
                dets.push(FaceBox {
                    x: x1 / INPUT_SIZE as f32,
                    y: y1 / INPUT_SIZE as f32,
                    w: (x2 - x1) / INPUT_SIZE as f32,
                    h: (y2 - y1) / INPUT_SIZE as f32,
                    score,
                });
            }
        }

        // 贪心 NMS：按置信度降序，保留与已选框 IoU < 阈值的框
        dets.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        let mut kept: Vec<FaceBox> = Vec::new();
        for b in dets {
            if kept.iter().all(|k| iou(k, &b) < NMS_IOU) {
                kept.push(b);
            }
        }
        Ok(kept)
    }
}

fn iou(a: &FaceBox, b: &FaceBox) -> f32 {
    let x0 = a.x.max(b.x);
    let y0 = a.y.max(b.y);
    let x1 = (a.x + a.w).min(b.x + b.w);
    let y1 = (a.y + a.h).min(b.y + b.h);
    let inter = (x1 - x0).max(0.0) * (y1 - y0).max(0.0);
    let union = a.w * a.h + b.w * b.h - inter;
    if union <= 0.0 {
        0.0
    } else {
        inter / union
    }
}
