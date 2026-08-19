//! 临时诊断工具：低阈值打印 YuNet 检测结果（M5 人脸漏检排查）
//! 用法: pic_process-debug-detect <JPG路径> [--threshold 0.2] [--full]
//! --full: 直接从原图 Triangle 缩放到 640（不经 1024 box 降采样），
//!         用于验证 box 平均是否磨掉小脸细节

use anyhow::Result;
use clap::Parser;
use pic_process::ai::facedetect::YuNet;
use pic_process::decode;

#[derive(Parser)]
struct Args {
    path: std::path::PathBuf,
    #[arg(long, default_value_t = 0.2)]
    threshold: f32,
    /// 原图直读模式（不经 1024 box 降采样）
    #[arg(long)]
    full: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mut yunet = YuNet::load(1)?;

    if args.full {
        // 原图全解码 → Triangle 640（验证 box 平均影响）
        let bytes = std::fs::read(&args.path)?;
        let mut dec = jpeg_decoder::Decoder::new(&bytes[..]);
        let pixels = dec.decode()?;
        let info = dec.info().unwrap();
        let (w, h) = (info.width as u32, info.height as u32);
        let img = image::RgbImage::from_raw(w, h, pixels).unwrap();
        let small = image::imageops::resize(&img, 640, 640, image::imageops::FilterType::Triangle);
        let boxes = yunet.detect_with_threshold(small.as_raw(), 640, 640, args.threshold)?;
        println!("[full 原图直读] 阈值 {:.2} 检测: {} 个人脸", args.threshold, boxes.len());
        for b in &boxes {
            println!(
                "  框=({:.3},{:.3},{:.3},{:.3}) 置信度={:.3}",
                b.x, b.y, b.w, b.h, b.score
            );
        }
        return Ok(());
    }

    let img = decode::load_analysis_image(&args.path)?.expect("解码失败");
    let boxes = yunet.detect_with_threshold(&img.rgb, img.width, img.height, args.threshold)?;
    println!("[流水线路径] 阈值 {:.2} 检测: {} 个人脸", args.threshold, boxes.len());
    for b in &boxes {
        println!(
            "  框=({:.3},{:.3},{:.3},{:.3}) 置信度={:.3}",
            b.x, b.y, b.w, b.h, b.score
        );
    }
    Ok(())
}
