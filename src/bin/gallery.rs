//! HTML 联系表生成器（M5 验证工具）
//!
//! 用法: pic_process-gallery <report.csv> -o gallery.html
//! 读取评分 CSV（score 子命令输出），为每张 JPG 生成缩略图（内嵌 base64），
//! 按总分排序输出单个 HTML 文件，供人工快速核对评分与观感。

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use rayon::prelude::*;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use pic_process::decode;
use pic_process::storage::FileRef;
use sha2::{Digest, Sha256};

#[derive(Parser)]
struct Args {
    /// score 子命令输出的 CSV
    report: PathBuf,
    /// 输出 HTML 路径
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// 照片根目录（默认使用 .firstcut 的上级目录，或 CSV 所在目录）
    #[arg(long)]
    photo_root: Option<PathBuf>,
    /// 缩略图长边像素
    #[arg(long, default_value_t = 320)]
    thumb: u32,
}

/// CSV 行（按表头名取值）
struct Row {
    path: String,
    filename: String,
    iso: String,
    faces: String,
    burst_size: String,
    burst_rank: String,
    burst_keep: String,
    /// 维度 -> 分数
    scores: HashMap<String, f64>,
    total: f64,
}

const SORT_SCRIPT: &str = r#"const t=document.getElementById('t'),tb=t.tBodies[0];
for (const [i,th] of [...t.tHead.rows[0].cells].entries()) {
  th.onclick=()=>{
    const rows=[...tb.rows].sort((a,b)=>{
      const x=a.cells[i].innerText.replace(/[^0-9.\-]/g,''),y=b.cells[i].innerText.replace(/[^0-9.\-]/g,'');
      const nx=parseFloat(x)||0,ny=parseFloat(y)||0;
      return x===y?a.cells[0].innerText.localeCompare(b.cells[0].innerText):ny-nx;
    });
    rows.forEach(r=>tb.appendChild(r));
  };
}"#;

fn html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn main() -> Result<()> {
    let args = Args::parse();
    anyhow::ensure!(
        (1..=1024).contains(&args.thumb),
        "缩略图尺寸必须在 1–1024 之间"
    );
    let report = std::path::absolute(&args.report)?;
    let parent = report.parent().context("评分报告没有父目录")?;
    let album = args.photo_root.clone().unwrap_or_else(|| {
        if parent
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case(".firstcut"))
        {
            parent.parent().unwrap_or(parent).to_path_buf()
        } else {
            parent.to_path_buf()
        }
    });
    let album = pic_process::review::normal_photo_root(&album)?;
    let output = args
        .output
        .clone()
        .unwrap_or_else(|| parent.join("gallery.html"));
    let output = std::path::absolute(output)?;

    let Some(report_file) = FileRef::at(&album, &report, false)?.open_read()? else {
        anyhow::bail!("评分报告不存在");
    };
    const MAX_REPORT_BYTES: u64 = 64 * 1024 * 1024;
    anyhow::ensure!(
        report_file.metadata()?.len() <= MAX_REPORT_BYTES,
        "评分报告超过大小上限"
    );
    use std::io::Read;
    let mut reader = csv::Reader::from_reader(report_file.take(MAX_REPORT_BYTES));
    let headers: Vec<String> = reader.headers()?.iter().map(|s| s.to_string()).collect();
    let col = |name: &str| -> Result<usize> {
        headers
            .iter()
            .position(|h| h == name)
            .with_context(|| format!("CSV 缺少列: {name}"))
    };
    let i_path = col("path")?;
    let i_name = col("filename")?;
    let i_raw = col("is_raw")?;
    let i_iso = col("iso")?;
    let i_faces = col("faces")?;
    let i_size = col("burst_size")?;
    let i_rank = col("burst_rank")?;
    let i_keep = col("burst_keep")?;
    let dims = [
        "sharpness_score",
        "exposure_score",
        "noise_score",
        "composition_score",
        "aesthetic_score",
    ];
    let i_dims: Vec<usize> = dims.iter().map(|d| col(d)).collect::<Result<_>>()?;
    let i_total = col("total_score")?;

    let mut rows: Vec<Row> = Vec::new();
    for rec in reader.records() {
        let rec = rec?;
        let get = |i: usize| rec.get(i).unwrap_or("").to_string();
        if get(i_raw) == "true" {
            continue; // 只展示 JPG（ARW 分数相同）
        }
        let mut scores = HashMap::new();
        for (d, &i) in dims.iter().zip(i_dims.iter()) {
            scores.insert(d.to_string(), get(i).parse().unwrap_or(0.0));
        }
        let total: f64 = get(i_total).parse().unwrap_or(0.0);
        let unescape = pic_process::output::csv::unescape_spreadsheet_text;
        rows.push(Row {
            path: unescape(&get(i_path)).to_string(),
            filename: unescape(&get(i_name)).to_string(),
            iso: unescape(&get(i_iso)).to_string(),
            faces: get(i_faces),
            burst_size: get(i_size),
            burst_rank: get(i_rank),
            burst_keep: get(i_keep),
            scores,
            total,
        });
    }

    // 按总分降序
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by(|&a, &b| rows[b].total.total_cmp(&rows[a].total));

    let thumbnail = |r: &Row| -> Option<String> {
        let path = std::path::Path::new(&r.path);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            album.join(path)
        };
        let ext = path.extension()?.to_str()?;
        if !(ext.eq_ignore_ascii_case("jpg") || ext.eq_ignore_ascii_case("jpeg")) {
            return None;
        }
        let file = FileRef::inside(&album, &path).ok()?.open_read().ok()??;
        let img = decode::load_analysis_file(file).ok().flatten()?;
        let (w, h) = (img.width, img.height);
        let scale = (args.thumb as f32 / w.max(h) as f32).min(1.0);
        let nw = ((w as f32 * scale).round() as u32).max(1);
        let nh = ((h as f32 * scale).round() as u32).max(1);
        let rgb = image::RgbImage::from_raw(w, h, img.rgb)?;
        let small = image::imageops::resize(&rgb, nw, nh, image::imageops::FilterType::Triangle);
        let mut buf = Vec::new();
        let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 70);
        enc.encode(small.as_raw(), nw, nh, image::ExtendedColorType::Rgb8)
            .ok()?;
        Some(STANDARD.encode(&buf))
    };

    FileRef::at(&album, &output, true)?.replace(|out| {
    writeln!(out, "<!DOCTYPE html><html lang=\"zh\"><head><meta charset=\"utf-8\">")?;
    let hash = STANDARD.encode(Sha256::digest(SORT_SCRIPT.as_bytes()));
    writeln!(out, "<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; img-src data:; style-src 'unsafe-inline'; script-src 'sha256-{hash}'; base-uri 'none'; form-action 'none'\">")?;
    writeln!(out, "<title>firstcut 评分联系表（{} 张）</title>", rows.len())?;
    writeln!(
        out,
        "<style>body{{font-family:system-ui;margin:16px;background:#1a1a1a;color:#ddd}}
        table{{border-collapse:collapse;width:100%}}
        th{{position:sticky;top:0;background:#333;cursor:pointer;padding:6px;font-size:13px}}
        td{{padding:6px;border-top:1px solid #333;vertical-align:top}}
        img{{max-width:320px;border-radius:4px}}
        .g5{{color:#4caf50;font-weight:bold}}.g4{{color:#8bc34a}}.g3{{color:#ffc107}}.g2{{color:#ff9800}}.g1{{color:#f44336}}
        .keep{{color:#4caf50}}.drop{{color:#f44336}}
        .score{{font-weight:bold;text-align:right}}</style></head><body>"
    )?;
    writeln!(out, "<h2>firstcut 评分联系表（{} 张，按总分降序，点击表头排序）</h2>", rows.len())?;
    writeln!(
        out,
        "<table id=\"t\"><thead><tr><th>照片</th><th>总分</th><th>清晰</th><th>曝光</th><th>噪点</th><th>构图</th><th>美学</th><th>ISO</th><th>人脸</th><th>连拍</th></tr></thead><tbody>"
    )?;
    for chunk in order.chunks(16) {
      // Keep at most one small batch of encoded thumbnails in memory.
      let rendered: Vec<_> = chunk.par_iter().map(|&i| thumbnail(&rows[i])).collect();
      for (&i, thumb) in chunk.iter().zip(rendered) {
        let r = &rows[i];
        let grade = if r.total >= 70.0 { "g5" } else if r.total >= 60.0 { "g4" } else if r.total >= 50.0 { "g3" } else if r.total >= 40.0 { "g2" } else { "g1" };
        let img_html = match thumb {
            Some(b64) => format!("<img loading=\"lazy\" src=\"data:image/jpeg;base64,{b64}\">"),
            None => "(缩略图失败)".to_string(),
        };
        let burst = if r.burst_rank.is_empty() {
            "—".to_string()
        } else if r.burst_keep == "true" {
            format!("<span class=\"keep\">组内 {}/{} ✓</span>", html(&r.burst_rank), html(&r.burst_size))
        } else {
            format!("<span class=\"drop\">组内 {}/{} ✗</span>", html(&r.burst_rank), html(&r.burst_size))
        };
        writeln!(
            out,
            "<tr><td>{img_html}<br><small>{}</small></td>\
             <td class=\"score {grade}\">{:.1}</td><td class=\"score\">{:.1}</td>\
             <td class=\"score\">{:.1}</td><td class=\"score\">{:.1}</td>\
             <td class=\"score\">{:.1}</td><td class=\"score\">{:.1}</td>\
             <td>{}</td><td>{}</td><td>{}</td></tr>",
            html(&r.filename),
            r.total,
            r.scores["sharpness_score"],
            r.scores["exposure_score"],
            r.scores["noise_score"],
            r.scores["composition_score"],
            r.scores["aesthetic_score"],
            html(&r.iso),
            html(&r.faces),
            burst,
        )?;
      }
    }
    writeln!(out, "</tbody></table>")?;
    write!(out, "<script>{SORT_SCRIPT}</script></body></html>")
    })?;
    eprintln!("[gallery] 已生成 {}（{} 张）", output.display(), rows.len());
    Ok(())
}
