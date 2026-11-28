use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
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
        /// 照片目录（或双目录模式下的 JPG 目录）
        dir: PathBuf,
        /// 双目录模式：指定独立 RAW 目录（与 `dir` 的 JPG 目录配对，支持子目录）
        #[arg(long)]
        raw_dir: Option<PathBuf>,
        /// 输出 CSV 路径（默认本机工作区状态目录的 scan-report.csv）
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// 写出未做表格公式保护的原始 CSV，供程序读取
        #[arg(long)]
        raw_csv: bool,
    },
    /// 扫描并评分（清晰度/曝光/噪点/构图/美学 + 连拍去重），输出 CSV
    Score {
        /// 照片目录（或双目录模式下的 JPG 目录）
        dir: PathBuf,
        /// 双目录模式：指定独立 RAW 目录（与 `dir` 的 JPG 目录配对，支持子目录）
        #[arg(long)]
        raw_dir: Option<PathBuf>,
        /// 输出 CSV 路径（默认本机工作区状态目录的 report.csv）
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// 写出未做表格公式保护的原始 CSV，供程序读取
        #[arg(long)]
        raw_csv: bool,
        /// 连拍保留单元（姿态簇/dHash 子簇）内保留前 K 张
        /// （缺省用 `[dedup] keep_k` 配置，默认 3）
        #[arg(short, long)]
        keep: Option<usize>,
        /// 跳过 AI 推理（无模型时快速预览）
        #[arg(long)]
        no_ai: bool,
        /// 写 XMP 星级侧车（仅单目录显式传入时写入源目录；双目录模式请用 export）
        #[arg(long)]
        xmp: bool,
        /// 显式将建议曝光写入 Adobe crs:Exposure2012 开发字段（需配合 --xmp，不覆盖已有人工曝光）
        #[arg(long)]
        xmp_exposure: bool,
        /// 增量缓存文件路径（默认本机工作区状态目录的 cache.sqlite）
        #[arg(long)]
        cache: Option<PathBuf>,
        /// 禁用增量缓存
        #[arg(long)]
        no_cache: bool,
        /// 评分配置文件（默认优先读取本机工作区状态目录的 config.toml）
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
        /// 照片目录（或双目录模式下的 JPG 目录）
        dir: PathBuf,
        /// 双目录模式：指定独立 RAW 目录（与 `dir` 的 JPG 目录配对，支持子目录）
        #[arg(long)]
        raw_dir: Option<PathBuf>,
        /// 评分配置（与 score 相同的 TOML；影响总分/星级/连拍划分）
        #[arg(long)]
        config: Option<PathBuf>,
        /// 增量缓存文件（默认本机工作区状态目录的 cache.sqlite）
        #[arg(long)]
        cache: Option<PathBuf>,
        /// 监听地址（容器部署时可用 0.0.0.0，并将宿主机端口限制为本机）
        #[arg(long, default_value = "127.0.0.1")]
        bind: IpAddr,
        /// 浏览器使用的完整地址（端口映射或 HTTPS 反代）；非回环监听需要 FIRSTCUT_ACCESS_TOKEN
        #[arg(long, env = "FIRSTCUT_PUBLIC_URL")]
        public_url: Option<String>,
        /// 监听端口
        #[arg(long, default_value_t = 8787)]
        port: u16,
        /// 不自动打开浏览器
        #[arg(long)]
        no_browser: bool,
    },
    /// 将筛选命中的 JPG/RAW/XMP 非破坏性复制到指定子目录或目标文件夹
    Export {
        /// 照片目录（或双目录模式下的 JPG 目录）
        dir: PathBuf,
        /// 双目录模式：指定独立 RAW 目录（与 `dir` 的 JPG 目录配对，支持子目录）
        #[arg(long)]
        raw_dir: Option<PathBuf>,
        /// 目标目录：单目录相对路径写入 <照片目录>/<路径>（默认 Selected），绝对路径写入该外部目录
        #[arg(long, default_value = "Selected")]
        to: PathBuf,
        /// 最低星级（1~5；与 --stars 互斥）
        #[arg(long)]
        min_stars: Option<u8>,
        /// 精确星级列表（逗号分隔，例如 --stars 4,5；与 --min-stars 互斥）
        #[arg(long, value_delimiter = ',')]
        stars: Option<Vec<u8>>,
        /// 仅保留有效帧（包含所有非连拍单张 + 连拍组中有效保留的帧）
        #[arg(long)]
        keep_only: bool,
        /// 仅导出有人脸的照片
        #[arg(long)]
        faces: bool,
        /// 仅导出人工标记为「留用 (pick)」的照片（等价 --flag pick）
        #[arg(long)]
        picked: bool,
        /// 按选片标记筛选：all / pick / exclude_reject / reject / unflagged（与 --picked 互斥）
        #[arg(long)]
        flag: Option<String>,
        /// 目录布局：split（默认 JPG/+RAW/）、flat（扁平化）、tree（保持原目录结构）
        #[arg(long, default_value = "split")]
        layout: String,
        /// 不复制 JPG（默认复制）
        #[arg(long)]
        no_jpg: bool,
        /// 不复制 RAW（默认复制）
        #[arg(long)]
        no_raw: bool,
        /// 不复制/生成 XMP 侧车（默认启用）
        #[arg(long)]
        no_xmp: bool,
        /// 仅为尚无源侧车、在导出目录中新生成的 XMP 附带 Adobe 建议曝光 crs:Exposure2012
        #[arg(long)]
        xmp_exposure: bool,
        /// 仅预览待复制文件与预计空间占用，不写磁盘
        #[arg(long)]
        dry_run: bool,
        /// 评分配置文件（默认优先读取本机工作区状态目录的 config.toml）
        #[arg(long)]
        config: Option<PathBuf>,
        /// 增量缓存文件（默认本机工作区状态目录的 cache.sqlite）
        #[arg(long)]
        cache: Option<PathBuf>,
    },
}

fn open_workspace(
    dir: &std::path::Path,
    raw_dir: Option<&std::path::Path>,
) -> Result<(
    pic_process::workspace::WorkspaceSource,
    pic_process::workspace::WorkspacePaths,
)> {
    let source = match raw_dir {
        Some(raw) => pic_process::workspace::WorkspaceSource::dual(dir, raw)?,
        None => pic_process::workspace::WorkspaceSource::single(dir)?,
    };
    let paths = pic_process::workspace::WorkspacePaths::for_source(&source)?;
    pic_process::workspace::import_legacy_state_if_needed(&source, &paths)?;
    Ok((source, paths))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Scan { dir, raw_dir, output, raw_csv } => {
            let (source, paths) = open_workspace(&dir, raw_dir.as_deref())?;
            let output = pic_process::storage::absolute_output(output)?.unwrap_or(paths.scan_report.clone());
            let entries = scan::scan_workspace(&source, None, None, true, false, &|_, _, _| {})?;
            eprintln!("[scan] 发现 {} 个文件（JPG/RAW）", entries.len());
            paths.ensure_state_dir()?;
            pic_process::output::csv::write_csv_in(&paths.state_dir, &output, &entries, !raw_csv)?;
            eprintln!("[scan] CSV 已写出: {}", output.display());
        }
        Commands::ConfigTemplate { output, preset } => {
            let current = pic_process::review::normal_photo_root(&std::env::current_dir()?)?;
            let output = pic_process::storage::absolute_output(output)?
                .unwrap_or_else(|| current.join(".firstcut/firstcut-template.toml"));
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
            pic_process::storage::FileRef::at(&current, &output, true)?.write(text.as_bytes())?;
            eprintln!("[config] 已写出: {}", output.display());
        }
        Commands::Score { dir, raw_dir, output, raw_csv, keep, no_ai, xmp, xmp_exposure, cache, no_cache, config, gpu } => {
            let (source, paths) = open_workspace(&dir, raw_dir.as_deref())?;
            paths.ensure_state_dir()?;
            let config = pic_process::storage::absolute_output(config)?;
            let (cfg, exists) = pic_process::review::load_review_config_in(&paths.state_dir, config.as_deref())?;
            let selected_config = config.as_ref().unwrap_or(&paths.config);
            if exists {
                eprintln!("[score] 配置已加载: {}", selected_config.display());
            } else {
                eprintln!("[score] 使用内置默认配置（工作区尚无 UI 配置）");
            }

            let preferences = pic_process::review::load_ui_preferences_in(&paths.state_dir);
            let include_raw = if source.is_dual() { true } else { preferences.include_raw };
            let opts = score::ScoreJobOptions {
                output_csv: pic_process::storage::absolute_output(output)?.unwrap_or(paths.report.clone()),
                raw_csv,
                cache_path: pic_process::storage::absolute_output(cache)?.unwrap_or(paths.cache.clone()),
                no_cache,
                no_ai,
                xmp,
                xmp_exposure,
                gpu,
                keep_override: keep,
                include_raw,
                skip_processed: true,
            };
            score::run_score_job_in_workspace(&source, &paths, &cfg, &opts, &|ev| match ev {
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
        Commands::Review { dir, raw_dir, config, cache, bind, public_url, port, no_browser } => {
            let (source, paths) = open_workspace(&dir, raw_dir.as_deref())?;
            let config = pic_process::storage::absolute_output(config)?;
            let (cfg, _) = pic_process::review::load_review_config_in(&paths.state_dir, config.as_deref())?;
            if config.is_some() {
                eprintln!("[review] 配置已加载: {}", config.as_ref().unwrap().display());
            }
            let cache = pic_process::storage::absolute_output(cache)?.unwrap_or(paths.cache.clone());
            pic_process::review::serve_with_bind_workspace(
                &source, &paths, &cfg, &cache, bind, port, !no_browser, config.as_deref(), public_url.as_deref(),
            )
                .with_context(|| "复核服务启动失败")?;
        }
        Commands::Export {
            dir,
            raw_dir,
            to,
            min_stars,
            stars,
            keep_only,
            faces,
            picked,
            flag,
            layout,
            no_jpg,
            no_raw,
            no_xmp,
            xmp_exposure,
            dry_run,
            config,
            cache,
        } => {
            let (source, paths) = open_workspace(&dir, raw_dir.as_deref())?;
            if source.is_dual() && !to.is_absolute() {
                anyhow::bail!("双目录模式下请通过 --to <本机绝对路径> 指定独立的处理目录，避免写入相机存储卡");
            }
            let config = pic_process::storage::absolute_output(config)?;
            let (cfg, _) = pic_process::review::load_review_config_in(&paths.state_dir, config.as_deref())?;
            let cache = pic_process::storage::absolute_output(cache)?.unwrap_or(paths.cache.clone());
            let layout = match layout.trim().to_ascii_lowercase().as_str() {
                "split" | "split_by_type" => pic_process::export::ExportLayout::SplitByType,
                "flat" => pic_process::export::ExportLayout::Flat,
                "tree" | "preserve_tree" => pic_process::export::ExportLayout::PreserveTree,
                other => anyhow::bail!("未知导出布局 {other:?}；可选: split, flat, tree"),
            };
            let target = if to.is_absolute() {
                pic_process::export::ExportTarget::ExternalDir(to)
            } else {
                pic_process::export::ExportTarget::AlbumSubdir(to)
            };
            if picked && flag.is_some() {
                anyhow::bail!("--picked 与 --flag 不能同时指定");
            }
            let flag_filter = if picked {
                pic_process::export::FlagFilter::PickedOnly
            } else if let Some(raw_flag) = &flag {
                pic_process::export::FlagFilter::parse(raw_flag).ok_or_else(|| {
                    anyhow::anyhow!(
                        "未知 --flag 值 {raw_flag:?}；可选: all, pick, exclude_reject, reject, unflagged"
                    )
                })?
            } else {
                pic_process::export::FlagFilter::All
            };
            let filter = pic_process::export::ExportFilter {
                stars,
                min_stars,
                keep_only,
                group_filter: pic_process::export::GroupFilter::All,
                faces_only: faces,
                flag_filter,
            };
            let (items, all_sources) =
                pic_process::export::collect_items_for_cli_in_workspace(&source, &paths, &cfg, &cache, &filter)?;
            let opts = pic_process::export::ExportOptions {
                target,
                layout,
                include_jpg: !no_jpg,
                include_raw: !no_raw,
                include_xmp: !no_xmp,
                xmp_exposure,
            };
            let plan = pic_process::export::plan_export_workspace(&source, &items, &all_sources, &opts)?;
            let p = &plan.preview;
            eprintln!(
                "[export] 目标: {} | 命中 {} 张照片 | 待写入: JPG {} / RAW {} / XMP {}（新生成 {}）| 跳过未变 {} | 保留既有 {} | 预计 {:.1} MB",
                p.target_display,
                p.selected_photos,
                p.jpg_to_copy,
                p.raw_to_copy,
                p.xmp_to_write,
                p.xmp_generated,
                p.skipped_unchanged,
                p.protected_existing,
                p.total_copy_bytes as f64 / (1024.0 * 1024.0),
            );
            if let Some(free) = p.available_bytes {
                eprintln!(
                    "[export] 目标磁盘可用空间: {:.2} GB",
                    free as f64 / (1024.0 * 1024.0 * 1024.0)
                );
            }
            if p.missing_raw_count > 0 {
                eprintln!("[export] 提示: {} 张照片无配对 RAW，已跳过其 RAW", p.missing_raw_count);
            }
            if p.renamed_count > 0 {
                eprintln!("[export] 提示: {} 张照片存在跨目录重名，已自动加目录前缀防覆盖", p.renamed_count);
            }
            for w in &p.warnings {
                eprintln!("[export] 警告: {w}");
            }
            if dry_run {
                eprintln!("[export] --dry-run 预览结束，未写入磁盘。");
                return Ok(());
            }
            let cancel = std::sync::atomic::AtomicBool::new(false);
            let summary = pic_process::export::run_export(&plan, &cancel, &|ev| match ev {
                pic_process::export::ExportEvent::Info(s) => eprintln!("[export] {s}"),
                pic_process::export::ExportEvent::Progress { done, total, .. } => {
                    if total > 0 && (done % 20 == 0 || done == total) {
                        eprintln!("[export] 进度 {done}/{total}");
                    }
                }
            })?;
            for conflict in &summary.conflicts {
                eprintln!("[export] 冲突（已保留既有文件）: {conflict}");
            }
            let conflict_note = if summary.conflicts.is_empty() {
                String::new()
            } else {
                format!("，冲突 {}", summary.conflicts.len())
            };
            eprintln!(
                "[export] 完成: 复制 JPG {} / RAW {} / XMP {}（共 {:.1} MB，跳过 {}，保留既有 {}，失败 {}{conflict_note}）→ {}",
                summary.jpg_copied,
                summary.raw_copied,
                summary.xmp_written,
                summary.bytes_written as f64 / (1024.0 * 1024.0),
                summary.skipped_unchanged,
                summary.protected_existing,
                summary.failed,
                summary.target_display,
            );
            if summary.failed > 0 {
                anyhow::bail!("导出完成但有 {} 个文件失败", summary.failed);
            }
        }
    }
    Ok(())
}
