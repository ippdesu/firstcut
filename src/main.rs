use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use pic_process::config::ScoreConfig;
use pic_process::scan::{self};
use pic_process::score;
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
    /// 扫描并评分（清晰度/曝光/噪点/构图/美学 + 连拍去重），输出 CSV
    Score {
        /// 照片目录
        dir: PathBuf,
        /// 输出 CSV 路径（默认 report.csv）
        #[arg(short, long, default_value = "report.csv")]
        output: PathBuf,
        /// 连拍保留单元（姿态簇/dHash 子簇）内保留前 K 张
        /// （缺省用 `[dedup] keep_k` 配置，默认 3）
        #[arg(short, long)]
        keep: Option<usize>,
        /// 跳过 AI 推理（无模型时快速预览）
        #[arg(long)]
        no_ai: bool,
        /// 写 XMP 星级侧车（xmp:Rating + firstcut 子分）
        #[arg(long)]
        xmp: bool,
        /// 增量缓存文件路径（默认 pic_process_cache.sqlite）
        #[arg(long, default_value = "pic_process_cache.sqlite")]
        cache: PathBuf,
        /// 禁用增量缓存
        #[arg(long)]
        no_cache: bool,
        /// 评分配置文件（TOML，可多场景存多份；缺省用内置默认）
        #[arg(long)]
        config: Option<PathBuf>,
        /// 实验性：用 DirectML GPU 推理（需 cargo build --features gpu；
        /// 失败自动回落 CPU）
        #[arg(long)]
        gpu: bool,
    },
    /// 输出默认评分配置模板（可存多份场景配置）
    ConfigTemplate {
        /// 输出路径（默认 firstcut.toml）
        #[arg(short, long, default_value = "firstcut.toml")]
        output: PathBuf,
        /// 输出内置场景预设而非通用模板（portrait/stage/highkey/sports/lowlight）
        #[arg(long, value_name = "名称")]
        preset: Option<String>,
    },
    /// 本地 Web 复核界面（缩略图墙 / 1:1 原图 / 连拍对比，浏览器打开）
    Review {
        /// 已用 score 评分过的照片目录
        dir: PathBuf,
        /// 评分配置（与 score 相同的 TOML；影响总分/星级/连拍划分）
        #[arg(long)]
        config: Option<PathBuf>,
        /// 增量缓存文件（应与 score 用的同一份）
        #[arg(long, default_value = "pic_process_cache.sqlite")]
        cache: PathBuf,
        /// 监听端口（仅绑定 127.0.0.1）
        #[arg(long, default_value_t = 8787)]
        port: u16,
        /// 不自动打开浏览器
        #[arg(long)]
        no_browser: bool,
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
        Commands::ConfigTemplate { output, preset } => {
            let text = match &preset {
                Some(name) => match pic_process::config::preset(name) {
                    Some(t) => t.to_string(),
                    None => {
                        let names: Vec<&str> =
                            pic_process::config::PRESETS.iter().map(|(n, _)| *n).collect();
                        anyhow::bail!(
                            "未知场景预设 {name:?}；可用: {}",
                            names.join(", ")
                        );
                    }
                },
                None => pic_process::config::config_template(),
            };
            std::fs::write(&output, text)?;
            eprintln!("[config] 已写出: {}", output.display());
        }
        Commands::Score { dir, output, keep, no_ai, xmp, cache, no_cache, config, gpu } => {
            // 0) 评分配置（默认或文件）
            //    显式传入的配置加载失败必须硬报错：静默回退默认值会让用户
            //    以为参数已生效（M6-4 同类问题的解析层版本）
            let cfg = match &config {
                Some(path) => pic_process::config::load_config(path).map_err(|err| {
                    anyhow::anyhow!("配置加载失败: {}\n{err:#}", path.display())
                })?,
                None => ScoreConfig::default(),
            };
            if config.is_some() {
                eprintln!("[score] 配置已加载: {}", config.as_ref().unwrap().display());
            }

            // 评分流水线在库内（score::run_score_job），review 界面的"重新评分"共用同一实现
            let opts = score::ScoreJobOptions {
                output_csv: output,
                cache_path: cache,
                no_cache,
                no_ai,
                xmp,
                gpu,
                keep_override: keep,
            };
            score::run_score_job(&dir, &cfg, &opts, &|ev| match ev {
                score::ScoreEvent::Info(s) => eprintln!("[score] {s}"),
                score::ScoreEvent::Progress { done, total } => {
                    eprintln!("[score] 进度 {}/{}", done, total)
                }
                score::ScoreEvent::Finished { total_jpg, hits, misses } => eprintln!(
                    "[score] 分析完成: {} 张 JPG（缓存命中 {}，新分析 {}）",
                    total_jpg, hits, misses
                ),
            })?;
        }
        Commands::Review { dir, config, cache, port, no_browser } => {
            // 与 score 相同的 fail-fast 配置加载
            let cfg = match &config {
                Some(path) => pic_process::config::load_config(path).map_err(|err| {
                    anyhow::anyhow!("配置加载失败: {}\n{err:#}", path.display())
                })?,
                None => ScoreConfig::default(),
            };
            if config.is_some() {
                eprintln!("[review] 配置已加载: {}", config.as_ref().unwrap().display());
            }
            pic_process::review::serve(&dir, &cfg, &cache, port, !no_browser, config.as_deref())
                .with_context(|| "复核服务启动失败")?;
        }
    }
    Ok(())
}
