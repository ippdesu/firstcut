use anyhow::Result;
use clap::{Parser, Subcommand};
use pic_process::config;
use pic_process::scan::{self, stem_of};
use pic_process::score;
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
    /// 扫描并对 JPG 做像素评分（清晰度/曝光/噪点），输出加权总分 CSV
    Score {
        /// 照片目录
        dir: PathBuf,
        /// 输出 CSV 路径（默认 report.csv）
        #[arg(short, long, default_value = "report.csv")]
        output: PathBuf,
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
        Commands::Score { dir, output } => {
            let mut entries = scan::scan_directory(&dir)?;
            eprintln!("[score] 发现 {} 个文件（JPG/ARW）", entries.len());

            // JPG 像素分析（Phase 1 约定：JPG 评分 → 映射到 ARW）
            let jpg_count = entries.iter().filter(|e| !e.is_raw).count();
            let analyzed = score::analyze_jpgs(&entries);
            eprintln!(
                "[score] 像素分析完成: {}/{} 张 JPG",
                analyzed.len(),
                jpg_count
            );

            // 回填 JPG 自身分数
            let by_stem: HashMap<String, score::PixelScores> = analyzed;
            for e in entries.iter_mut().filter(|e| !e.is_raw) {
                if let Some(s) = by_stem.get(&stem_of(&e.filename)) {
                    apply_scores(e, s);
                }
            }
            // JPG 分数映射到同名 ARW
            for e in entries.iter_mut().filter(|e| e.is_raw) {
                if let Some(s) = by_stem.get(&stem_of(&e.filename)) {
                    apply_scores(e, s);
                }
            }

            pic_process::output::csv::write_csv(&output, &entries)?;
            eprintln!("[score] CSV 已写出: {}", output.display());
        }
    }
    Ok(())
}

/// 把分数写入 PhotoEntry 的 CSV 字段
fn apply_scores(e: &mut scan::PhotoEntry, s: &score::PixelScores) {
    e.sharpness_score = fmt(s.sharpness);
    e.exposure_score = fmt(s.exposure);
    e.noise_score = fmt(s.noise);
    e.total_score = fmt(score::total_score(s, &config::ScoreWeights::default()));
}

fn fmt(v: f64) -> String {
    format!("{:.1}", v)
}
