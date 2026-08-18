//! 调参工具（M1/M5 用）：输出每张照片的原始指标，用于校准评分曲线
//!
//! 用法: pic_process-tune <目录> [-o metrics.csv]
//! 输出列: filename, iso, width, height, tenengrad_var, dark_noise, over_pct, under_pct, mean_luma

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Parser;
use rayon::prelude::*;

use pic_process::decode;
use pic_process::metrics;
use pic_process::scan;

#[derive(Parser)]
struct Args {
    dir: PathBuf,
    #[arg(short, long, default_value = "metrics.csv")]
    output: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let entries = scan::scan_directory(&args.dir)?;
    let jpgs: Vec<_> = entries.iter().filter(|e| !e.is_raw).collect();

    let mut wtr = csv::Writer::from_path(&args.output)?;
    wtr.write_record([
        "filename", "iso", "width", "height", "tenengrad_var", "luma_var",
        "norm_sharp", "dark_noise", "over_pct", "under_pct", "mean_luma",
    ])?;

    let rows: Vec<Vec<String>> = jpgs
        .par_iter()
        .filter_map(|e| {
            let img = decode::load_analysis_image(Path::new(&e.path)).ok().flatten()?;
            let var = metrics::sharpness::tenengrad_variance(&img);
            let norm = metrics::sharpness::normalized_sharpness(var, img.luma_variance);
            let noise = metrics::noise::dark_noise_metric(&img);
            let stats = metrics::exposure::exposure_stats(&img);
            let iso = e.iso.parse::<u32>().unwrap_or(100);
            Some(vec![
                e.filename.clone(),
                iso.to_string(),
                img.width.to_string(),
                img.height.to_string(),
                format!("{:.1}", var),
                format!("{:.1}", img.luma_variance),
                format!("{:.3}", norm),
                format!("{:.2}", noise),
                format!("{:.4}", stats.over_ratio),
                format!("{:.4}", stats.under_ratio),
                format!("{:.1}", stats.mean),
            ])
        })
        .collect();

    for r in rows {
        wtr.write_record(&r)?;
    }
    wtr.flush()?;
    eprintln!("[tune] 已写出 {}", args.output.display());

    // 汇总
    let all: Vec<Vec<String>> = {
        let mut reader = csv::Reader::from_path(&args.output)?;
        reader.records().map(|r| r.unwrap().iter().map(|s| s.to_string()).collect()).collect()
    };
    if !all.is_empty() {
        let stat = |idx: usize| -> (f64, f64, f64) {
            let mut v: Vec<f64> = all.iter().map(|r| r[idx].parse().unwrap_or(0.0)).collect();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let med = v[v.len() / 2];
            (v[0], med, v[v.len() - 1])
        };
        eprintln!("[tune] tenengrad_var  min/med/max: {:?}", stat(4));
        eprintln!("[tune] luma_var       min/med/max: {:?}", stat(5));
        eprintln!("[tune] norm_sharp     min/med/max: {:?}", stat(6));
        eprintln!("[tune] dark_noise     min/med/max: {:?}", stat(7));
        eprintln!("[tune] mean_luma      min/med/max: {:?}", stat(10));
    }
    Ok(())
}
