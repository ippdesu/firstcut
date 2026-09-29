use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use pic_process::config::ScoreConfig;
use pic_process::scan::{self};
use pic_process::score;
use std::net::IpAddr;
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
        /// 输出 CSV 路径（默认 <照片目录>/.firstcut/scan-report.csv）
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// 扫描并评分（清晰度/曝光/噪点/构图/美学 + 连拍去重），输出 CSV
    Score {
        /// 照片目录
        dir: PathBuf,
        /// 输出 CSV 路径（默认 <照片目录>/.firstcut/report.csv）
        #[arg(short, long)]
        output: Option<PathBuf>,
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
        /// 增量缓存文件路径（默认 <照片目录>/.firstcut/cache.sqlite）
        #[arg(long)]
        cache: Option<PathBuf>,
        /// 禁用增量缓存
        #[arg(long)]
        no_cache: bool,
        /// 评分配置文件（默认优先读取 <照片目录>/.firstcut/config.toml）
        #[arg(long)]
        config: Option<PathBuf>,
        /// 实验性：用 DirectML GPU 推理（需 cargo build --features gpu；
        /// 失败自动回落 CPU）
        #[arg(long)]
        gpu: bool,
    },
    /// 输出默认评分配置模板（可存多份场景配置）
    ConfigTemplate {
        /// 输出路径（默认当前工作目录下的 .firstcut/firstcut-template.toml）
        #[arg(short, long)]
        output: Option<PathBuf>,
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
        /// 增量缓存文件（默认 <照片目录>/.firstcut/cache.sqlite）
        #[arg(long)]
        cache: Option<PathBuf>,
        /// 监听地址（容器部署时可用 0.0.0.0，并将宿主机端口限制为本机）
        #[arg(long, default_value = "127.0.0.1")]
        bind: IpAddr,
        /// 监听端口
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
            let dir = pic_process::review::normal_photo_root(&dir)?;
            let paths = pic_process::review::photo_root_paths(&dir);
            let output = output.unwrap_or(paths.scan_report);
            if let Some(parent) = output.parent() { std::fs::create_dir_all(parent)?; }
            let entries = scan::scan_directory(&dir)?;
            eprintln!("[scan] 发现 {} 个文件（JPG/ARW）", entries.len());
            pic_process::output::csv::write_csv(&output, &entries)?;
            eprintln!("[scan] CSV 已写出: {}", output.display());
        }
        Commands::ConfigTemplate { output, preset } => {
            let output = output.unwrap_or_else(|| PathBuf::from(".firstcut/firstcut-template.toml"));
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
            if let Some(parent) = output.parent() { std::fs::create_dir_all(parent)?; }
            std::fs::write(&output, text)?;
            eprintln!("[config] 已写出: {}", output.display());
        }
        Commands::Score { dir, output, keep, no_ai, xmp, cache, no_cache, config, gpu } => {
            let dir = pic_process::review::normal_photo_root(&dir)?;
            let paths = pic_process::review::photo_root_paths(&dir);
            std::fs::create_dir_all(&paths.state_dir)?;
            // 0) 评分配置（默认或文件）
            //    显式传入的配置加载失败必须硬报错：静默回退默认值会让用户
            //    以为参数已生效（M6-4 同类问题的解析层版本）
            let selected_config = config.as_ref().unwrap_or(&paths.config);
            let cfg = if selected_config.exists() {
                pic_process::config::load_config(selected_config).map_err(|err| {
                    anyhow::anyhow!("配置加载失败: {}\n{err:#}", selected_config.display())
                })?
            } else if config.is_some() {
                anyhow::bail!("配置加载失败: {}\n文件不存在", selected_config.display());
            } else {
                ScoreConfig::default()
            };
            if selected_config.exists() {
                eprintln!("[score] 配置已加载: {}", selected_config.display());
            } else {
                eprintln!("[score] 使用内置默认配置（照片根目录尚无 UI 配置）");
            }

            // 评分流水线在库内（score::run_score_job），review 界面的"重新评分"共用同一实现
            let preferences = pic_process::review::load_ui_preferences(&dir);
            let opts = score::ScoreJobOptions {
                output_csv: output.unwrap_or(paths.report),
                cache_path: cache.unwrap_or(paths.cache),
                no_cache,
                no_ai,
                xmp,
                gpu,
                keep_override: keep,
                include_raw: preferences.include_raw,
                skip_processed: true,
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
        Commands::Review { dir, config, cache, bind, port, no_browser } => {
            let dir = pic_process::review::normal_photo_root(&dir)?;
            let paths = pic_process::review::photo_root_paths(&dir);
            // 与 score 相同的 fail-fast 配置加载
            let selected_config = config.as_ref().unwrap_or(&paths.config);
            let cfg = if selected_config.exists() {
                pic_process::config::load_config(selected_config).map_err(|err| {
                    anyhow::anyhow!("配置加载失败: {}\n{err:#}", selected_config.display())
                })?
            } else if config.is_some() {
                anyhow::bail!("配置加载失败: {}\n文件不存在", selected_config.display());
            } else {
                ScoreConfig::default()
            };
            if config.is_some() {
                eprintln!("[review] 配置已加载: {}", config.as_ref().unwrap().display());
            }
            let cache = cache.unwrap_or(paths.cache);
            pic_process::review::serve_with_bind(
                &dir, &cfg, &cache, bind, port, !no_browser, config.as_deref(),
            )
                .with_context(|| "复核服务启动失败")?;
        }
    }
    Ok(())
}
