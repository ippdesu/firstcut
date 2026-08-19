//! HTML 联系表生成器（M5 验证工具）
//!
//! 用法: pic_process-gallery <report.csv> -o gallery.html
//! 读取评分 CSV（score 子命令输出），为每张 JPG 生成缩略图（内嵌 base64），
//! 按总分排序输出单个 HTML 文件，供人工快速核对评分与观感。

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use rayon::prelude::*;

use pic_process::decode;

#[derive(Parser)]
struct Args {
    /// score 子命令输出的 CSV
    report: PathBuf,
    /// 输出 HTML 路径
    #[arg(short, long, default_value = "gallery.html")]
    output: PathBuf,
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

fn main() -> Result<()> {
    let args = Args::parse();

    // 读取 CSV（首行为表头）
    let mut reader = csv::Reader::from_path(&args.report)?;
    let headers: Vec<String> = reader.headers()?.iter().map(|s| s.to_string()).collect();
    let col = |name: &str| -> usize {
        headers
            .iter()
            .position(|h| h == name)
            .unwrap_or_else(|| panic!("CSV 缺少列: {name}"))
    };
    let i_path = col("path");
    let i_name = col("filename");
    let i_raw = col("is_raw");
    let i_iso = col("iso");
    let i_faces = col("faces");
    let i_size = col("burst_size");
    let i_rank = col("burst_rank");
    let i_keep = col("burst_keep");
    let dims = ["sharpness_score", "exposure_score", "noise_score", "composition_score", "aesthetic_score"];
    let i_dims: Vec<usize> = dims.iter().map(|d| col(d)).collect();
    let i_total = col("total_score");

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
        rows.push(Row {
            path: get(i_path),
            filename: get(i_name),
            iso: get(i_iso),
            faces: get(i_faces),
            burst_size: get(i_size),
            burst_rank: get(i_rank),
            burst_keep: get(i_keep),
            scores,
            total,
        });
    }

    // 缩略图（并行生成 base64）
    let thumbs: Vec<Option<String>> = rows
        .par_iter()
        .map(|r| {
            let img = decode::load_analysis_image(std::path::Path::new(&r.path)).ok().flatten()?;
            let (w, h) = (img.width, img.height);
            let scale = args.thumb as f32 / w.max(h) as f32;
            if scale >= 1.0 {
                return None;
            }
            let nw = ((w as f32 * scale).round() as u32).max(1);
            let nh = ((h as f32 * scale).round() as u32).max(1);
            let rgb = image::RgbImage::from_raw(w, h, img.rgb)?;
            let small =
                image::imageops::resize(&rgb, nw, nh, image::imageops::FilterType::Triangle);
            let mut buf = Vec::new();
            let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 70);
            let _ = enc.encode(small.as_raw(), nw, nh, image::ExtendedColorType::Rgb8);
            Some(base64_encode(&buf))
        })
        .collect();

    // 按总分降序
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by(|&a, &b| {
        rows[b].total.partial_cmp(&rows[a].total).unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut out = std::fs::File::create(&args.output)?;
    writeln!(out, "<!DOCTYPE html><html lang=\"zh\"><head><meta charset=\"utf-8\">")?;
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
    for &i in &order {
        let r = &rows[i];
        let grade = if r.total >= 70.0 { "g5" } else if r.total >= 60.0 { "g4" } else if r.total >= 50.0 { "g3" } else if r.total >= 40.0 { "g2" } else { "g1" };
        let img_html = match &thumbs[i] {
            Some(b64) => format!("<img loading=\"lazy\" src=\"data:image/jpeg;base64,{b64}\">"),
            None => "(缩略图失败)".to_string(),
        };
        let burst = if r.burst_rank.is_empty() {
            "—".to_string()
        } else if r.burst_keep == "true" {
            format!("<span class=\"keep\">组内 {}/{} ✓</span>", r.burst_rank, r.burst_size)
        } else {
            format!("<span class=\"drop\">组内 {}/{} ✗</span>", r.burst_rank, r.burst_size)
        };
        writeln!(
            out,
            "<tr><td>{img_html}<br><small>{}</small></td>\
             <td class=\"score {grade}\">{:.1}</td><td class=\"score\">{:.1}</td>\
             <td class=\"score\">{:.1}</td><td class=\"score\">{:.1}</td>\
             <td class=\"score\">{:.1}</td><td class=\"score\">{:.1}</td>\
             <td>{}</td><td>{}</td><td>{}</td></tr>",
            r.filename,
            r.total,
            r.scores["sharpness_score"],
            r.scores["exposure_score"],
            r.scores["noise_score"],
            r.scores["composition_score"],
            r.scores["aesthetic_score"],
            r.iso,
            r.faces,
            burst,
        )?;
    }
    writeln!(out, "</tbody></table>")?;
    write!(
        out,
        "{}",
        r#"<script>
        const t=document.getElementById('t'),tb=t.tBodies[0];
        [...t.tHead.rows[0].cells].forEach((th,i)=>{th.onclick=()=>{
          const rows=[...tb.rows].sort((a,b)=>{
            const x=a.cells[i].innerText.replace(/[^0-9.\-]/g,''),y=b.cells[i].innerText.replace(/[^0-9.\-]/g,'');
            const nx=parseFloat(x)||0,ny=parseFloat(y)||0;
            return x===y?(a.cells[0].innerText<b.cells[0].innerText?-1:1):ny-nx;
          });rows.forEach(r=>tb.appendChild(r));
        }});
        </script></body></html>"#
    )?;
    eprintln!("[gallery] 已生成 {}（{} 张）", args.output.display(), rows.len());
    Ok(())
}

fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}
