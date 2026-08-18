mod output;
mod scan;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "pic_process", version, about = "索尼照片初筛评分工具")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 扫描目录，读取 EXIF 建立照片索引，输出 CSV 报告
    Scan {
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
            output::csv::write_csv(&output, &entries)?;
            eprintln!("[scan] CSV 已写出: {}", output.display());
        }
    }
    Ok(())
}
