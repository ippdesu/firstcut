//! 评分汇总（M1）：像素分析调度 + 加权总分

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use rayon::prelude::*;

use crate::config::{MetricParams, ScoreWeights};
use crate::decode;
use crate::metrics;
use crate::scan::{PhotoEntry, stem_of};

/// 单张照片的三维像素分数（0-100）
#[derive(Debug, Clone, Copy, Default)]
pub struct PixelScores {
    pub sharpness: f64,
    pub exposure: f64,
    pub noise: f64,
}

/// 加权总分（0-100，1 位小数）
pub fn total_score(s: &PixelScores, w: &ScoreWeights) -> f64 {
    let total = s.sharpness * w.sharpness + s.exposure * w.exposure + s.noise * w.noise;
    (total * 10.0).round() / 10.0
}

/// 对全部 JPG 并行做像素分析，返回 stem -> 分数
pub fn analyze_jpgs(entries: &[PhotoEntry]) -> HashMap<String, PixelScores> {
    let params = MetricParams::default();
    let counter = AtomicUsize::new(0);
    let total = entries.iter().filter(|e| !e.is_raw).count().max(1);

    entries
        .par_iter()
        .filter(|e| !e.is_raw)
        .filter_map(|e| {
            let done = counter.fetch_add(1, Ordering::Relaxed) + 1;
            if done % 25 == 0 || done == total {
                eprintln!("[score] 进度 {}/{}", done, total);
            }
            let scores = analyze_one(e, &params)?;
            Some((stem_of(&e.filename), scores))
        })
        .collect()
}

/// 单张 JPG 的三维像素评分；非 JPG/解码失败返回 None
pub fn analyze_one(e: &PhotoEntry, p: &MetricParams) -> Option<PixelScores> {
    let img = decode::load_analysis_image(Path::new(&e.path)).ok().flatten()?;

    let sharp_var = metrics::sharpness::tenengrad_variance(&img);
    let norm = metrics::sharpness::normalized_sharpness(sharp_var, img.luma_variance);
    let sharpness = metrics::sharpness::sharpness_score(norm, p.sharpness_k);

    let stats = metrics::exposure::exposure_stats(&img);
    let exposure = metrics::exposure::exposure_score(&stats);

    let iso = e.iso.parse::<u32>().unwrap_or(100);
    let noise_metric = metrics::noise::dark_noise_metric(&img);
    let noise = metrics::noise::noise_score(noise_metric, iso, p.noise_k0);

    Some(PixelScores {
        sharpness,
        exposure,
        noise,
    })
}
