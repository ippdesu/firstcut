use anyhow::Result;
use clap::{Parser, Subcommand};
use pic_process::config::{DedupParams, ScoreWeights};
use pic_process::dedup::{self, BurstInfo};
use pic_process::scan::{self, stem_of};
use pic_process::score::{self, AnalysisResult};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "pic_process", version, about = "索尼照片初筛评分工具")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 扫描目录，读取 EXIF 建立照片索引，输出 CSV 报告（不含评分）
    Scan {
        /// 照片目录
        dir: PathBuf,
        /// 输出 CSV 路径（默认 report.csv）
        #[arg(short, long, default_value = "report.csv")]
        output: PathBuf,
    },
    /// 扫描并对 JPG 做像素评分 + 连拍去重，输出加权总分 CSV
    Score {
        /// 照片目录
        dir: PathBuf,
        /// 输出 CSV 路径（默认 report.csv）
        #[arg(short, long, default_value = "report.csv")]
        output: PathBuf,
        /// 连拍子簇内保留前 K 张
        #[arg(short, long, default_value_t = 2)]
        keep: usize,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Scan { dir, output } => {
            let entries = scan::scan_directory(&dir)?;
            eprintln!("[scan] 发现 {} 个文件（JPG/ARW）", entries.len());
            pic_process::output::csv::write_csv(&output, &entries)?;
            eprintln!("[scan] CSV 已写出: {}", output.display());
        }
        Commands::Score { dir, output, keep } => {
            let mut entries = scan::scan_directory(&dir)?;
            eprintln!("[score] 发现 {} 个文件（JPG/ARW）", entries.len());

            // 1) JPG 像素分析（Phase 1 约定：JPG 评分 → 映射到 ARW）
            let jpg_count = entries.iter().filter(|e| !e.is_raw).count();
            let analyzed = score::analyze_jpgs(&entries);
            eprintln!(
                "[score] 像素分析完成: {}/{} 张 JPG",
                analyzed.len(),
                jpg_count
            );

            // 2) 连拍去重（只针对有分析的 JPG）
            let dedup_params = DedupParams { keep_k: keep, ..Default::default() };
            let burst_info = run_burst_analysis(&mut entries, &analyzed, &dedup_params);

            // 3) 回填 JPG 分数 + 连拍信息
            let by_stem: HashMap<String, AnalysisResult> = analyzed;
            for e in entries.iter_mut().filter(|e| !e.is_raw) {
                let stem = stem_of(&e.filename);
                if let Some(r) = by_stem.get(&stem) {
                    apply_scores(e, &r.scores);
                }
                if let Some(info) = burst_info.get(&stem) {
                    apply_burst(e, info);
                }
            }
            // 4) JPG 结果映射到同名 ARW
            for e in entries.iter_mut().filter(|e| e.is_raw) {
                let stem = stem_of(&e.filename);
                if let Some(r) = by_stem.get(&stem) {
                    apply_scores(e, &r.scores);
                }
                if let Some(info) = burst_info.get(&stem) {
                    apply_burst(e, info);
                }
            }

            pic_process::output::csv::write_csv(&output, &entries)?;
            eprintln!("[score] CSV 已写出: {}", output.display());
        }
    }
    Ok(())
}

/// 对 JPG 条目做连拍分析，返回 stem -> BurstInfo
fn run_burst_analysis(
    entries: &mut [scan::PhotoEntry],
    analyzed: &HashMap<String, AnalysisResult>,
    params: &DedupParams,
) -> HashMap<String, BurstInfo> {
    // 取有分析的 JPG（保持扫描顺序）
    let jpg_idx: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| !e.is_raw && analyzed.contains_key(&stem_of(&e.filename)))
        .map(|(i, _)| i)
        .collect();
    if jpg_idx.len() < 2 {
        return HashMap::new();
    }
    let jpg_entries: Vec<scan::PhotoEntry> =
        jpg_idx.iter().map(|&i| entries[i].clone()).collect();
    let hashes: Vec<u64> = jpg_idx
        .iter()
        .map(|&i| analyzed[&stem_of(&entries[i].filename)].dhash)
        .collect();
    let scores: Vec<f64> = jpg_idx
        .iter()
        .map(|&i| {
            let s = analyzed[&stem_of(&entries[i].filename)].scores;
            score::total_score(&s, &ScoreWeights::default())
        })
        .collect();

    let infos = dedup::analyze_bursts(&jpg_entries, &hashes, &scores, params);
    let mut map = HashMap::new();
    for (&idx, info) in jpg_idx.iter().zip(infos.iter()) {
        map.insert(stem_of(&entries[idx].filename), info.clone());
    }
    let burst_groups = infos.iter().filter(|i| i.group != 0).count();
    eprintln!(
        "[score] 连拍去重: {} 张 JPG 中 {} 张属于连拍组",
        jpg_idx.len(),
        burst_groups
    );
    map
}

/// 把分数写入 PhotoEntry 的 CSV 字段
fn apply_scores(e: &mut scan::PhotoEntry, s: &score::PixelScores) {
    e.sharpness_score = fmt(s.sharpness);
    e.exposure_score = fmt(s.exposure);
    e.noise_score = fmt(s.noise);
    e.total_score = fmt(score::total_score(s, &ScoreWeights::default()));
}

/// 把连拍信息写入 PhotoEntry 的 CSV 字段
fn apply_burst(e: &mut scan::PhotoEntry, info: &BurstInfo) {
    e.burst_group = if info.group == 0 { "0".into() } else { info.group.to_string() };
    e.burst_size = if info.size == 0 { String::new() } else { info.size.to_string() };
    e.burst_rank = if info.rank == 0 { String::new() } else { info.rank.to_string() };
    e.burst_keep = if info.size == 0 { String::new() } else { info.keep.to_string() };
}

fn fmt(v: f64) -> String {
    format!("{:.1}", v)
}
