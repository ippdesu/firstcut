//! 构图评分（M3）：基于人脸检测结果
//!
//! 维度：
//! - 位置：人脸中心离三分法交点（1/3,1/3 等）的距离
//! - 大小：人脸高度占画面比例（太小=主体不突出，太大=怼脸裁切）
//! - 数量：多人合影轻微降权
//! 无人脸（风景/静物）时给中性分，不惩罚。

use crate::ai::facedetect::FaceBox;

/// 三分法交点（归一化坐标）
const THIRDS: [(f32, f32); 4] = [(1.0 / 3.0, 1.0 / 3.0), (1.0 / 3.0, 2.0 / 3.0), (2.0 / 3.0, 1.0 / 3.0), (2.0 / 3.0, 2.0 / 3.0)];

/// 无人脸时的中性分
const NO_FACE_SCORE: f64 = 60.0;

/// 构图分数（0-100）
pub fn composition_score(faces: &[FaceBox]) -> f64 {
    if faces.is_empty() {
        return NO_FACE_SCORE;
    }

    let mut best = 0.0f64;
    for f in faces {
        let cx = f.x + f.w / 2.0;
        let cy = f.y + f.h / 2.0;
        // 到最近三分交点的距离（最大可能距离约 0.47）
        let d = THIRDS
            .iter()
            .map(|&(tx, ty)| ((cx - tx).powi(2) + (cy - ty).powi(2)).sqrt())
            .fold(f32::MAX, f32::min);
        let pos_score = (1.0 - d / 0.47).clamp(0.0, 1.0) as f64;

        // 人脸高度占比
        let size_pct = (f.h * 100.0) as f64;
        let size_score = if size_pct < 2.0 {
            0.3 // 太远，主体不突出
        } else if size_pct < 8.0 {
            0.5 + (size_pct / 8.0) * 0.5
        } else if size_pct <= 30.0 {
            1.0 // 理想区间
        } else {
            0.75 // 怼脸/裁切风险
        };

        let s = pos_score * size_score;
        if s > best {
            best = s;
        }
    }

    // 多人合影轻微降权（5 人以上明显降）
    let count_factor = match faces.len() {
        1 => 1.0,
        2..=4 => 0.95,
        _ => 0.85,
    };
    (100.0 * best * count_factor).clamp(0.0, 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn face(x: f32, y: f32, w: f32, h: f32) -> FaceBox {
        FaceBox { x, y, w, h, score: 0.9 }
    }

    #[test]
    fn no_face_neutral() {
        assert_eq!(composition_score(&[]), 60.0);
    }

    #[test]
    fn centered_face_lower_than_thirds() {
        // 居中（0.4,0.4,0.2,0.25）vs 三分交点（0.28,0.28,0.2,0.25）
        let centered = composition_score(&[face(0.4, 0.4, 0.2, 0.25)]);
        let thirds = composition_score(&[face(0.28, 0.28, 0.2, 0.25)]);
        assert!(thirds > centered, "三分法位置应更高: {thirds} vs {centered}");
    }

    #[test]
    fn ideal_size_scores_high() {
        // 人脸中心恰好落在三分交点 (1/3,1/3)：x=1/3-0.1=0.2333, y=1/3-0.125=0.2083
        let s = composition_score(&[face(0.2333, 0.2083, 0.2, 0.25)]);
        assert!(s > 95.0, "理想位置+理想大小应接近满分: {s}");
    }

    #[test]
    fn tiny_face_penalized() {
        let tiny = composition_score(&[face(0.28, 0.28, 0.03, 0.03)]);
        let normal = composition_score(&[face(0.28, 0.28, 0.2, 0.25)]);
        assert!(tiny < normal, "人脸过小应降分: {tiny} vs {normal}");
    }
}
