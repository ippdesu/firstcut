//! 本地 Web 复核界面入口（M-UI1）：`pic_process review <目录>`
//!
//! 只读：加载配置（fail-fast）→ 构建快照（scan + 缓存）→ 启动 axum 服务
//! 并打开浏览器。除缩略图缓存外不写任何文件。

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "pic_process-review", about = "本地 Web 复核界面（缩略图墙 / 1:1 原图 / 连拍对比）")]
struct Args {
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
}

fn main() -> Result<()> {
    let args = Args::parse();
    let cfg = match &args.config {
        Some(path) => pic_process::config::load_config(path)
            .map_err(|err| anyhow::anyhow!("配置加载失败: {}\n{err:#}", path.display()))?,
        None => pic_process::config::ScoreConfig::default(),
    };
    if args.config.is_some() {
        eprintln!("[review] 配置已加载: {}", args.config.as_ref().unwrap().display());
    }

    pic_process::review::serve(&args.dir, &cfg, &args.cache, args.port, !args.no_browser)
        .context("复核服务启动失败")?;
    Ok(())
}
