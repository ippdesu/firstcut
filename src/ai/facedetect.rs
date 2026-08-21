//! 人脸检测（M3/M5）：SCRFD 10g（InsightFace 架构，小脸检测显著优于 YuNet）
//!
//! models/scrfd_10g_bnkps.onnx（RuteNL/SCRFD-face-detection-ONNX，hf-mirror 下载）：
//! - 输入 1x3x640x640 RGB，(x-127.5)/128 归一化
//! - 9 个输出：score/bbox/kps × stride [8,16,32]（score 已 sigmoid）
//! - bbox 为 distance(l,t,r,b)×stride，从格中心 (gx*stride, gy*stride) 解码
//!   （每格 num_anchors=2 共享中心；参考 insightface model_zoo/scrfd.py）
//! - 阈值 0.3（小脸/远距人像场景；insightface 默认 0.5 但分析图仅 1024px）

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

pub struct Scrfd {
    session: Session,
}

/// 模型输入边长
const INPUT_SIZE: usize = 640;
/// 三个特征层下采样步长
const STRIDES: [usize; 3] = [8, 16, 32];
/// 每格 anchor 数
const NUM_ANCHORS: usize = 2;
/// 置信度阈值（小脸/远距人像场景，0.5 会漏检）
const SCORE_THRESHOLD: f32 = 0.3;
/// NMS IoU 阈值
const NMS_IOU: f32 = 0.3;

impl Scrfd {
    /// 加载模型；`intra_threads` 为 ORT 内部线程数
    pub fn load(intra_threads: usize) -> Result<Self> {
        let builder = Session::builder().map_err(crate::ai::ort_err)?;
        let session = builder
            .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level3)
            .map_err(crate::ai::ort_err)?
            .with_intra_threads(intra_threads)
            .map_err(crate::ai::ort_err)?
            .commit_from_file(format!("{}/scrfd_10g_bnkps.onnx", crate::ai::MODELS_DIR))
            .map_err(crate::ai::ort_err)?;
        Ok(Self { session })
    }

    /// 检测人脸，返回归一化人脸框列表（已解码 + NMS）
    pub fn detect(&mut self, rgb: &[u8], w: u32, h: u32) -> Result<Vec<FaceBox>> {
        let img = image::RgbImage::from_raw(w, h, rgb.to_vec())
            .ok_or_else(|| anyhow::anyhow!("RGB 数据尺寸不一致"))?;
        let small = image::DynamicImage::ImageRgb8(img).resize_exact(
            INPUT_SIZE as u32,
            INPUT_SIZE as u32,
            image::imageops::FilterType::Triangle,
        );
        let px = small.into_rgb8();

        // 预处理：RGB、(x-127.5)/128
        let mut arr = Array4::<f32>::zeros((1, 3, INPUT_SIZE, INPUT_SIZE));
        for y in 0..INPUT_SIZE {
            for x in 0..INPUT_SIZE {
                let p = px.get_pixel(x as u32, y as u32);
                arr[[0, 0, y, x]] = (p[0] as f32 - 127.5) / 128.0;
                arr[[0, 1, y, x]] = (p[1] as f32 - 127.5) / 128.0;
                arr[[0, 2, y, x]] = (p[2] as f32 - 127.5) / 128.0;
            }
        }

        let tensor = ort::value::Tensor::from_array(arr).map_err(crate::ai::ort_err)?;
        let input = ort::inputs!["input.1" => tensor];
        let out = self.session.run(input).map_err(crate::ai::ort_err)?;

        // 解码各尺度候选（insightface scrfd.py 同款逻辑）
        let mut dets: Vec<FaceBox> = Vec::new();
        for (si, &stride) in STRIDES.iter().enumerate() {
            let grid = INPUT_SIZE / stride;
            let n = grid * grid * NUM_ANCHORS;

            let (_, scores) = out[si].try_extract_tensor::<f32>().map_err(crate::ai::ort_err)?;
            let (_, bbox) = out[si + 3].try_extract_tensor::<f32>().map_err(crate::ai::ort_err)?;
            if scores.len() < n || bbox.len() < n * 4 {
                anyhow::bail!("SCRFD 输出尺寸异常 (stride={stride})");
            }

            for i in 0..n {
                let s = scores[i];
                if s < SCORE_THRESHOLD {
                    continue;
                }
                let gi = i / NUM_ANCHORS;
                let gx = (gi % grid) as f32;
                let gy = (gi / grid) as f32;
                let (cx, cy) = (gx * stride as f32, gy * stride as f32);
                let b4 = i * 4;
                let l = bbox[b4] * stride as f32;
                let t = bbox[b4 + 1] * stride as f32;
                let r = bbox[b4 + 2] * stride as f32;
                let b = bbox[b4 + 3] * stride as f32;
                let x1 = (cx - l).clamp(0.0, INPUT_SIZE as f32);
                let y1 = (cy - t).clamp(0.0, INPUT_SIZE as f32);
                let x2 = (cx + r).clamp(0.0, INPUT_SIZE as f32);
                let y2 = (cy + b).clamp(0.0, INPUT_SIZE as f32);
                dets.push(FaceBox {
                    x: x1 / INPUT_SIZE as f32,
                    y: y1 / INPUT_SIZE as f32,
                    w: (x2 - x1) / INPUT_SIZE as f32,
                    h: (y2 - y1) / INPUT_SIZE as f32,
                    score: s,
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
