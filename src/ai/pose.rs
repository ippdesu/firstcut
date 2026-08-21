//! 人体姿态检测（M5）：YOLOv8n-pose（Ultralytics，Xenova ONNX 转换）
//!
//! 用途：人脸检测（SCRFD）漏检时，用人体框/头部关键点定位主体区域，
//! 供"主体区域锐度"评估——覆盖"人体占画面大但人脸小"的大光圈人像场景。
//!
//! models/yolov8n_pose.onnx：
//! - 输入 1x3x640x640 RGB，/255 归一化
//! - 输出 [1, 56, 8400]：每列 [cx, cy, w, h, cls(person), kps 17×3(x,y,conf)]
//!   （cls 与 kps conf 为 logits 需 sigmoid；坐标已解码到输入像素空间）

use anyhow::Result;
use ndarray::Array4;
use ort::session::Session;

/// 人体检测结果（坐标归一化 0-1）
#[derive(Debug, Clone)]
pub struct PersonBox {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub score: f32,
    /// 17 个 COCO 关键点 (x, y, conf)，索引 0=鼻子 1=左眼 2=右眼 3=左耳 4=右耳
    pub keypoints: [(f32, f32, f32); 17],
}

pub struct PoseDet {
    session: Session,
}

/// 模型输入边长
const INPUT_SIZE: usize = 640;
/// 输出列数（4 box + 1 cls + 17*3 kps）
const COLS: usize = 56;
/// anchor 总数（80² + 40² + 20²）
const ANCHORS: usize = 8400;
/// 人体置信度阈值
const CONF_THRESHOLD: f32 = 0.25;
/// NMS IoU 阈值
const NMS_IOU: f32 = 0.5;

impl PoseDet {
    pub fn load(intra_threads: usize) -> Result<Self> {
        let builder = Session::builder().map_err(crate::ai::ort_err)?;
        let session = builder
            .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level3)
            .map_err(crate::ai::ort_err)?
            .with_intra_threads(intra_threads)
            .map_err(crate::ai::ort_err)?
            .commit_from_file(format!("{}/yolov8n_pose.onnx", crate::ai::MODELS_DIR))
            .map_err(crate::ai::ort_err)?;
        Ok(Self { session })
    }

    /// 检测人体，返回人体框 + 关键点列表（已 NMS）
    pub fn detect(&mut self, rgb: &[u8], w: u32, h: u32) -> Result<Vec<PersonBox>> {
        let img = image::RgbImage::from_raw(w, h, rgb.to_vec())
            .ok_or_else(|| anyhow::anyhow!("RGB 数据尺寸不一致"))?;
        let small = image::DynamicImage::ImageRgb8(img).resize_exact(
            INPUT_SIZE as u32,
            INPUT_SIZE as u32,
            image::imageops::FilterType::Triangle,
        );
        let px = small.into_rgb8();

        // 预处理：RGB、/255
        let mut arr = Array4::<f32>::zeros((1, 3, INPUT_SIZE, INPUT_SIZE));
        for y in 0..INPUT_SIZE {
            for x in 0..INPUT_SIZE {
                let p = px.get_pixel(x as u32, y as u32);
                arr[[0, 0, y, x]] = p[0] as f32 / 255.0;
                arr[[0, 1, y, x]] = p[1] as f32 / 255.0;
                arr[[0, 2, y, x]] = p[2] as f32 / 255.0;
            }
        }

        let tensor = ort::value::Tensor::from_array(arr).map_err(crate::ai::ort_err)?;
        let input = ort::inputs!["images" => tensor];
        let out = self.session.run(input).map_err(crate::ai::ort_err)?;
        let (_, data) = out[0].try_extract_tensor::<f32>().map_err(crate::ai::ort_err)?;
        if data.len() < COLS * ANCHORS {
            anyhow::bail!("YOLOv8-pose 输出尺寸异常: {}", data.len());
        }

        let mut persons: Vec<PersonBox> = Vec::new();
        for i in 0..ANCHORS {
            // flat 布局 [1,56,8400]（通道优先）：anchor i 的特征 j 位于 data[j*8400 + i]
            // Xenova 转换：score 已 sigmoid、坐标已在输入像素空间（见仓库 README）
            let cls = data[4 * ANCHORS + i];
            if cls < CONF_THRESHOLD {
                continue;
            }
            let cx = data[i];
            let cy = data[ANCHORS + i];
            let bw = data[2 * ANCHORS + i];
            let bh = data[3 * ANCHORS + i];
            let x1 = ((cx - bw / 2.0) / INPUT_SIZE as f32).clamp(0.0, 1.0);
            let y1 = ((cy - bh / 2.0) / INPUT_SIZE as f32).clamp(0.0, 1.0);
            let x2 = ((cx + bw / 2.0) / INPUT_SIZE as f32).clamp(0.0, 1.0);
            let y2 = ((cy + bh / 2.0) / INPUT_SIZE as f32).clamp(0.0, 1.0);
            let mut keypoints = [(0.0f32, 0.0f32, 0.0f32); 17];
            for k in 0..17 {
                let kx = data[(5 + k * 3) * ANCHORS + i] / INPUT_SIZE as f32;
                let ky = data[(6 + k * 3) * ANCHORS + i] / INPUT_SIZE as f32;
                let kc = data[(7 + k * 3) * ANCHORS + i];
                keypoints[k] = (kx, ky, kc);
            }
            persons.push(PersonBox {
                x: x1,
                y: y1,
                w: (x2 - x1).max(0.0),
                h: (y2 - y1).max(0.0),
                score: cls,
                keypoints,
            });
        }

        // NMS
        persons.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        let mut kept: Vec<PersonBox> = Vec::new();
        for p in persons {
            if kept.iter().all(|k| iou_person(k, &p) < NMS_IOU) {
                kept.push(p);
            }
        }
        Ok(kept)
    }
}

/// 由关键点 0-4（鼻/眼/耳）计算头部包围盒（扩展 1.4x），返回归一化 (cx, cy, half_w, half_h)
/// 关键点不足时返回 None
pub fn head_region(p: &PersonBox) -> Option<(f64, f64, f64, f64)> {
    let pts: Vec<(f32, f32)> = p.keypoints[..5]
        .iter()
        .filter(|&&(_, _, c)| c > 0.3)
        .map(|&(x, y, _)| (x, y))
        .collect();
    if pts.len() < 2 {
        return None;
    }
    let min_x = pts.iter().map(|p| p.0).fold(f32::MAX, f32::min);
    let max_x = pts.iter().map(|p| p.0).fold(f32::MIN, f32::max);
    let min_y = pts.iter().map(|p| p.1).fold(f32::MAX, f32::min);
    let max_y = pts.iter().map(|p| p.1).fold(f32::MIN, f32::max);
    let (cx, cy) = ((min_x + max_x) / 2.0, (min_y + max_y) / 2.0);
    let (half_w, half_h) = ((max_x - min_x) * 1.4 / 2.0 + 0.02, (max_y - min_y) * 1.6 / 2.0 + 0.03);
    Some((cx as f64, cy as f64, half_w as f64, half_h as f64))
}

fn iou_person(a: &PersonBox, b: &PersonBox) -> f32 {
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
