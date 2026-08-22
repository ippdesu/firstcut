//! CLIPIQA 美学/质量评分（M5，替换 MUSIQ）
//!
//! 86Cao/IQA-ONNX-Models 的 CLIP-IQA+ 变体（learned prompts 已烘焙进模型）：
//! - 输入 224x224 RGB，CLIP 归一化 (x/255 - mean) / std
//! - 输出 [1,1] 质量分（0-1，sigmoid 后），×100 为 0-100 分

use anyhow::Result;
use ndarray::Array4;
use ort::session::Session;

pub struct ClipIqa {
    session: Session,
}

/// CLIP 图像归一化参数
const MEAN: [f32; 3] = [0.48145466, 0.4578275, 0.40821073];
const STD: [f32; 3] = [0.26862954, 0.26130258, 0.27577711];

impl ClipIqa {
    pub fn load(intra_threads: usize) -> Result<Self> {
        let builder = Session::builder().map_err(crate::ai::ort_err)?;
        let session = builder
            .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level3)
            .map_err(crate::ai::ort_err)?
            .with_intra_threads(intra_threads)
            .map_err(crate::ai::ort_err)?
            .commit_from_file(format!("{}/clipiqa_model.onnx", crate::ai::MODELS_DIR))
            .map_err(crate::ai::ort_err)?;
        Ok(Self { session })
    }

    /// 对 RGB 图像评分，返回 0-100（需 &mut：onnxruntime run 语义）
    pub fn score(&mut self, rgb: &[u8], w: u32, h: u32) -> Result<f64> {
        let img = image::RgbImage::from_raw(w, h, rgb.to_vec())
            .ok_or_else(|| anyhow::anyhow!("RGB 数据尺寸不一致"))?;
        let small = image::DynamicImage::ImageRgb8(img)
            .resize_exact(224, 224, image::imageops::FilterType::Triangle);
        let px = small.into_rgb8();

        // 预处理：CLIP 归一化 (x/255 - mean) / std（批量操作）
        let raw = px.into_raw();
        let pixels = 224 * 224;
        let mut arr_data = Vec::with_capacity(1 * 3 * 224 * 224);
        for c in 0..3 {
            let offset = c * pixels;
            let mean = MEAN[c];
            let std = STD[c];
            for i in 0..pixels {
                arr_data.push((raw[offset + i] as f32 / 255.0 - mean) / std);
            }
        }
        let arr = Array4::<f32>::from_shape_vec((1, 3, 224, 224), arr_data)
            .map_err(crate::ai::ort_err)?;

        let tensor = ort::value::Tensor::from_array(arr).map_err(crate::ai::ort_err)?;
        let input = ort::inputs!["input" => tensor];
        let out = self.session.run(input).map_err(crate::ai::ort_err)?;
        let (_, data) = out[0].try_extract_tensor::<f32>().map_err(crate::ai::ort_err)?;
        let v = data.iter().copied().next().unwrap_or(0.5) as f64;
        Ok(v.clamp(0.0, 1.0) * 100.0)
    }
}
