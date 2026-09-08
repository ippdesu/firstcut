use anyhow::Result;
use clap::{Parser, Subcommand};
use pic_process::cache::ScoreCache;
use pic_process::config::{DedupParams, ScoreConfig, ScoreWeights};
use pic_process::dedup::{self, BurstInfo};
use pic_process::output;
use pic_process::scan::{self};
use pic_process::score::{self, AiEngine, AnalysisResult};
use std::collections::{HashMap, HashSet};
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
        /// 连拍子簇内保留前 K 张
        #[arg(short, long, default_value_t = 2)]
        keep: usize,
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
        Commands::Score { dir, output, keep, no_ai, xmp, cache, no_cache, config } => {
            // 0) 评分配置（默认或文件）
            let cfg = match &config {
                Some(path) => match pic_process::config::load_config(path) {
                    Ok(c) => {
                        eprintln!("[score] 配置已加载: {}", path.display());
                        c
                    }
                    Err(err) => {
                        eprintln!("[score] 警告: 配置加载失败（使用默认）: {err:#}");
                        ScoreConfig::default()
                    }
                },
                None => ScoreConfig::default(),
            };

            let mut entries = scan::scan_directory(&dir)?;
            eprintln!("[score] 发现 {} 个文件（JPG/ARW）", entries.len());

            // 0) AI 引擎（可跳过）
            let engine = if no_ai {
                eprintln!("[score] 跳过 AI 推理（--no-ai）");
                AiEngine::none()
            } else {
                match AiEngine::load() {
                    Ok(e) => e,
                    Err(err) => {
                        eprintln!("[score] 警告: {err:#}");
                        eprintln!("[score] 降级为纯像素评分；或使用 --no-ai 关闭提示");
                        AiEngine::none()
                    }
                }
            };

            // 1) 增量缓存（可禁用）
            let mut cache = if no_cache {
                None
            } else {
                match ScoreCache::open(&cache, pic_process::config::config_fingerprint(&cfg)) {
                    Ok(c) => {
                        eprintln!("[score] 缓存: {} 条（{}）", c.len(), cache.display());
                        Some(c)
                    }
                    Err(err) => {
                        eprintln!("[score] 警告: {err:#}（本次不使用缓存）");
                        None
                    }
                }
            };

            // 2) JPG 像素分析 + AI（优先缓存命中；并行段只读快照）
            let jpg_count = entries.iter().filter(|e| !e.is_raw).count();
            let cache_rows = cache.as_ref().map(|c| c.rows());
            let outcome = score::analyze_jpgs(&entries, &engine, cache_rows, &cfg);
            eprintln!(
                "[score] 分析完成: {} 张 JPG（缓存命中 {}，新分析 {}）",
                jpg_count, outcome.hits, outcome.misses
            );
            // 新结果落盘
            if let Some(c) = &mut cache {
                for (path, size, mtime, r) in &outcome.new_rows {
                    let _ = c.put(path, *size, *mtime, r);
                }
                if let Err(err) = c.flush() {
                    eprintln!("[score] 警告: 缓存写入失败: {err:#}");
                }
            }

            // 3) 连拍去重（只针对有分析的 JPG）
            let dedup_params = DedupParams { keep_k: keep, ..Default::default() };
            let burst_info =
                run_burst_analysis(&mut entries, &outcome.results, &dedup_params, &cfg.weights);

            // 4) 回填 JPG 分数 + 连拍信息
            let by_key: HashMap<String, AnalysisResult> = outcome.results;
            for e in entries.iter_mut().filter(|e| !e.is_raw) {
                let key = e.pair_key();
                if let Some(r) = by_key.get(&key) {
                    apply_scores(e, &r.scores, r.faces, &cfg);
                }
                if let Some(info) = burst_info.get(&key) {
                    apply_burst(e, info);
                }
            }
            // 5) JPG 结果映射到同名 ARW（同一目录下同名才算配对）
            for e in entries.iter_mut().filter(|e| e.is_raw) {
                let key = e.pair_key();
                if let Some(r) = by_key.get(&key) {
                    apply_scores(e, &r.scores, r.faces, &cfg);
                }
                if let Some(info) = burst_info.get(&key) {
                    apply_burst(e, info);
                }
            }

            // 6) 星级（批次内相对排名或绝对阈值，见 config.star_mode）
            //    同一配对键的 JPG/ARW 共用同一星级
            let rated: Vec<(String, f64)> = by_key
                .iter()
                .map(|(k, r)| (k.clone(), score::total_score(&r.scores, &cfg.weights)))
                .collect();
            let ratings = output::xmp::assign_ratings(&rated, &cfg.metric);
            for e in entries.iter_mut() {
                if let Some(st) = ratings.get(&e.pair_key()) {
                    e.stars = st.to_string();
                }
            }

            // 7) XMP 星级侧车（可选；同一配对键只写一次，JPG/ARW 共用）
            if xmp {
                let mut written = 0usize;
                let mut skipped = 0usize;
                let mut seen: HashSet<String> = HashSet::new();
                for e in &entries {
                    let key = e.pair_key();
                    if !seen.insert(key.clone()) {
                        continue;
                    }
                    if let Some(r) = by_key.get(&key) {
                        let total = score::total_score(&r.scores, &cfg.weights);
                        let rating = *ratings.get(&key).unwrap_or(&3);
                        match output::xmp::write_sidecar(e, &r.scores, total, rating) {
                            Ok(true) => written += 1,
                            Ok(false) => skipped += 1,
                            Err(err) => eprintln!("[xmp] 写入失败 {}: {err:#}", e.path),
                        }
                    }
                }
                eprintln!("[xmp] 侧车写入 {written} 个，跳过 {skipped} 个");
            }

            pic_process::output::csv::write_csv(&output, &entries)?;
            eprintln!("[score] CSV 已写出: {}", output.display());
        }
    }
    Ok(())
}

/// 对 JPG 条目做连拍分析，返回 配对键 -> BurstInfo
fn run_burst_analysis(
    entries: &mut [scan::PhotoEntry],
    analyzed: &HashMap<String, AnalysisResult>,
    params: &DedupParams,
    weights: &ScoreWeights,
) -> HashMap<String, BurstInfo> {
    // 取有分析的 JPG（保持扫描顺序）
    let jpg_idx: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| !e.is_raw && analyzed.contains_key(&e.pair_key()))
        .map(|(i, _)| i)
        .collect();
    if jpg_idx.len() < 2 {
        return HashMap::new();
    }
    let jpg_entries: Vec<scan::PhotoEntry> =
        jpg_idx.iter().map(|&i| entries[i].clone()).collect();
    let hashes: Vec<u64> =
        jpg_idx.iter().map(|&i| analyzed[&entries[i].pair_key()].dhash).collect();
    let scores: Vec<f64> = jpg_idx
        .iter()
        .map(|&i| {
            let s = analyzed[&entries[i].pair_key()].scores;
            score::total_score(&s, weights)
        })
        .collect();

    let infos = dedup::analyze_bursts(&jpg_entries, &hashes, &scores, params);
    let mut map = HashMap::new();
    for (&idx, info) in jpg_idx.iter().zip(infos.iter()) {
        map.insert(entries[idx].pair_key(), info.clone());
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
fn apply_scores(e: &mut scan::PhotoEntry, s: &score::PixelScores, faces: usize, cfg: &ScoreConfig) {
    e.sharpness_score = fmt(s.sharpness);
    e.exposure_score = fmt(s.exposure);
    e.noise_score = fmt(s.noise);
    e.composition_score = fmt(s.composition);
    e.aesthetic_score = fmt(s.aesthetic);
    e.total_score = fmt(score::total_score(s, &cfg.weights));
    e.faces = faces.to_string();
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
