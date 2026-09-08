//! 临时诊断工具：曝光指标 vs 主体（人脸）区域亮度（M6 调参用）
//!
//! 用法: pic_process-debug-exposure <JPG路径> [<JPG路径> ...]
//!
//! 输出每张图的全图亮度统计（mean / 截尾均值 / 分位数）
//! 以及每个主体级人脸框（h ≥ 4%）的亮度统计（mean / p50），
//! 用于判断"全图均值型曝光指标"在暗背景/白背景场景下的误判。

use anyhow::Result;
use pic_process::ai::facedetect::Scrfd;
use pic_process::decode;
use pic_process::metrics;

/// 直方图分位数（0-255）
fn percentile(hist: &[u64], q: f64) -> f64 {
    let total: u64 = hist.iter().sum();
    if total == 0 {
        return 0.0;
    }
    let need = (total as f64 * q).ceil() as u64;
    let mut acc = 0u64;
    for (v, c) in hist.iter().enumerate() {
        acc += c;
        if acc >= need {
            return v as f64;
        }
    }
    255.0
}

/// 归一化矩形（0-1）内的亮度直方图
fn region_hist(luma: &[u8], w: u32, h: u32, x: f32, y: f32, bw: f32, bh: f32) -> Vec<u64> {
    let w = w as usize;
    let h = h as usize;
    let mut hist = vec![0u64; 256];
    if w < 2 || h < 2 || luma.len() < w * h {
        return hist;
    }
    let x0 = ((x * w as f32) as usize).min(w - 1);
    let x1 = (((x + bw) * w as f32) as usize).min(w - 1);
    let y0 = ((y * h as f32) as usize).min(h - 1);
    let y1 = (((y + bh) * h as f32) as usize).min(h - 1);
    for yy in y0..=y1 {
        let row = yy * w;
        for xx in x0..=x1 {
            hist[luma[row + xx] as usize] += 1;
        }
    }
    hist
}

fn main() -> Result<()> {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    if paths.is_empty() {
        eprintln!("用法: pic_process-debug-exposure <JPG路径> ...");
        std::process::exit(2);
    }
    let mut scrfd = Scrfd::load(4, false)?;

    for path in &paths {
        let Some(img) = decode::load_analysis_image(std::path::Path::new(path))? else {
            println!("{path}: 解码失败");
            continue;
        };
        let stats = metrics::exposure::exposure_stats(&img);
        println!(
            "== {path}\n   全图 mean={:.1} mean_trunc={:.1} p50={:.0} p85={:.0} p95={:.0} over={:.2}% under={:.2}%",
            stats.mean,
            stats.mean_trunc,
            percentile(&img.histogram, 0.50),
            percentile(&img.histogram, 0.85),
            percentile(&img.histogram, 0.95),
            stats.over_ratio * 100.0,
            stats.under_ratio * 100.0,
        );
        let boxes = scrfd.detect(&img.rgb, img.width, img.height)?;
        let mut subj = 0usize;
        for f in &boxes {
            if f.h < 0.04 {
                continue;
            }
            subj += 1;
            let hist = region_hist(&img.luma, img.width, img.height, f.x, f.y, f.w, f.h);
            let n: u64 = hist.iter().sum();
            let mean: f64 =
                hist.iter().enumerate().map(|(v, c)| v as f64 * *c as f64).sum::<f64>() / n as f64;
            println!(
                "   主体脸 #{subj} h={:.1}% conf={:.3} 框内 mean={:.1} p50={:.0} p85={:.0}",
                f.h * 100.0,
                f.score,
                mean,
                percentile(&hist, 0.50),
                percentile(&hist, 0.85),
            );
        }
        if subj == 0 {
            println!("   无主体级人脸（共 {} 个检出框）", boxes.len());
        }
    }
    Ok(())
}
