//! MUSIQ 美学/质量评分（M3）
//!
//! 输入：224x224 RGB，归一化 (x/255 - 0.5)/0.5（Inception 风格，即 [-1,1]）
//! 输出：0-100 质量分（MUSIQ 训练于 AVA/KonIQ 等数据集，兼具美学与技术质量）

use anyhow::Result;
use ndarray::Array4;
use ort::session::Session;

pub struct Musiq {
    session: Session,
}

impl Musiq {
    pub fn load() -> Result<Self> {
        let builder = Session::builder().map_err(crate::ai::ort_err)?;
        let session = builder
            .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level3)
            .map_err(crate::ai::ort_err)?
            .commit_from_file(format!("{}/musiq_model.onnx", crate::ai::MODELS_DIR))
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

        let mut arr = Array4::<f32>::zeros((1, 3, 224, 224));
        for y in 0..224usize {
            for x in 0..224usize {
                let p = px.get_pixel(x as u32, y as u32);
                arr[[0, 0, y, x]] = (p[0] as f32 / 255.0 - 0.5) / 0.5;
                arr[[0, 1, y, x]] = (p[1] as f32 / 255.0 - 0.5) / 0.5;
                arr[[0, 2, y, x]] = (p[2] as f32 / 255.0 - 0.5) / 0.5;
            }
        }

        let tensor = ort::value::Tensor::from_array(arr).map_err(crate::ai::ort_err)?;
        let input = ort::inputs!["input" => tensor];
        let out = self.session.run(input).map_err(crate::ai::ort_err)?;
        let (_, data) = out[0].try_extract_tensor::<f32>().map_err(crate::ai::ort_err)?;
        let v = data.iter().copied().next().unwrap_or(50.0) as f64;
        Ok(v.clamp(0.0, 100.0))
    }
}
