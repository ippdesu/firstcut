//! 临时诊断工具：SCRFD 检测验证（M5 换模型用）
//! 用法: pic_process-debug-scrfd <JPG路径> [--threshold 0.5]
//! 解码规范来自 insightface model_zoo/scrfd.py：
//!   预处理 BGR、(x-127.5)/128；score 已 sigmoid；
//!   bbox 为 distance(l,t,r,b)×stride，从格中心 (gx*stride, gy*stride) 解码；
//!   每格 num_anchors=2

use anyhow::Result;
use clap::Parser;
use ndarray::Array4;
use ort::session::Session;
use pic_process::decode;

#[derive(Parser)]
struct Args {
    path: std::path::PathBuf,
    #[arg(long, default_value_t = 0.5)]
    threshold: f32,
    /// 额外打印最大人脸的姿态描述子（10 维，M9 定标用）
    #[arg(long)]
    desc: bool,
}

const INPUT_SIZE: usize = 640;
const STRIDES: [usize; 3] = [8, 16, 32];

fn main() -> Result<()> {
    let args = Args::parse();
    let mut builder = Session::builder().map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let mut session = builder
        .commit_from_file("models/scrfd_10g_bnkps.onnx")
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;

    let img = decode::load_analysis_image(&args.path)?.expect("解码失败");
    let small = image::DynamicImage::ImageRgb8(
        image::RgbImage::from_raw(img.width, img.height, img.rgb).unwrap(),
    )
    .resize_exact(INPUT_SIZE as u32, INPUT_SIZE as u32, image::imageops::FilterType::Triangle);
    let px = small.into_rgb8();

    // 预处理：(x-127.5)/128、RGB 顺序（cv2 读图 BGR + swapRB=True → 模型期望 RGB）
    let mut arr = Array4::<f32>::zeros((1, 3, INPUT_SIZE, INPUT_SIZE));
    for y in 0..INPUT_SIZE {
        for x in 0..INPUT_SIZE {
            let p = px.get_pixel(x as u32, y as u32);
            arr[[0, 0, y, x]] = (p[0] as f32 - 127.5) / 128.0; // R
            arr[[0, 1, y, x]] = (p[1] as f32 - 127.5) / 128.0; // G
            arr[[0, 2, y, x]] = (p[2] as f32 - 127.5) / 128.0; // B
        }
    }
    let tensor = ort::value::Tensor::from_array(arr).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let input = ort::inputs!["input.1" => tensor];
    let out = session.run(input).map_err(|e| anyhow::anyhow!("{e:?}"))?;

    let mut dets: Vec<(f32, f32, f32, f32, f32, [(f32, f32); 5])> = Vec::new(); // x1,y1,x2,y2,score,kps
    for (si, &stride) in STRIDES.iter().enumerate() {
        let (_, scores) = out[si].try_extract_tensor::<f32>().map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let (_, bbox) = out[si + 3].try_extract_tensor::<f32>().map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let (_, kps) = out[si + 6].try_extract_tensor::<f32>().map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let grid = INPUT_SIZE / stride;
        let n = scores.len();
        for i in 0..n {
            let s = scores[i];
            if s < args.threshold {
                continue;
            }
            let gi = i / 2; // num_anchors=2
            let gx = (gi % grid) as f32;
            let gy = (gi / grid) as f32;
            let (cx, cy) = (gx * stride as f32, gy * stride as f32);
            let b4 = i * 4;
            let raw = [
                bbox[b4],
                bbox[b4 + 1],
                bbox[b4 + 2],
                bbox[b4 + 3],
            ];
            let (l, t, r, b) = (
                raw[0] * stride as f32,
                raw[1] * stride as f32,
                raw[2] * stride as f32,
                raw[3] * stride as f32,
            );
            let (x1, y1) = ((cx - l).max(0.0), (cy - t).max(0.0));
            let (x2, y2) = ((cx + r).min(640.0), (cy + b).min(640.0));
            // kps：格中心 + offset×stride（与 bbox 同一距离约定）
            let k10 = i * 10;
            let mut pts = [(0.0f32, 0.0f32); 5];
            for (j, p) in pts.iter_mut().enumerate() {
                let px = (cx + kps[k10 + j * 2] * stride as f32) / 640.0;
                let py = (cy + kps[k10 + j * 2 + 1] * stride as f32) / 640.0;
                *p = (px, py);
            }
            eprintln!(
                "[raw] stride={stride} i={i} grid=({gx},{gy}) center=({cx:.1},{cy:.1}) dist=({:.2},{:.2},{:.2},{:.2}) -> box=({x1:.1},{y1:.1},{x2:.1},{y2:.1}) s={s:.3}\n      kps_raw=[{:.2},{:.2} {:.2},{:.2} {:.2},{:.2} {:.2},{:.2} {:.2},{:.2}]",
                raw[0], raw[1], raw[2], raw[3],
                kps[k10], kps[k10+1], kps[k10+2], kps[k10+3], kps[k10+4], kps[k10+5],
                kps[k10+6], kps[k10+7], kps[k10+8], kps[k10+9],
            );
            dets.push((x1, y1, x2, y2, s, pts));
        }
    }
    // 简单 NMS（按分数降序）
    dets.sort_by(|a, b| b.4.partial_cmp(&a.4).unwrap());
    let mut kept: Vec<(f32, f32, f32, f32, f32, [(f32, f32); 5])> = Vec::new();
    for d in dets {
        if kept.iter().all(|k| iou5(k, &d) < 0.3) {            kept.push(d);
        }
    }
    println!("阈值 {:.2} 检测: {} 个人脸", args.threshold, kept.len());
    // kps 解码自检：5 个关键点应落在人脸框（放大 20% 容差）内。
    // 若距离约定弄错（如忘了 ×stride 或误用绝对坐标），这里会大面积外落。
    // 注意 kept 里是 640 空间像素坐标、pts 是归一化坐标，统一到归一化再比。
    let mut inside = 0usize;
    let mut total_pts = 0usize;
    for (x1, y1, x2, y2, s, pts) in &kept {
        let (nx1, ny1, nw, nh) = (x1 / 640.0, y1 / 640.0, (x2 - x1) / 640.0, (y2 - y1) / 640.0);
        let (ax1, ay1, ax2, ay2) = (
            nx1 - nw * 0.2,
            ny1 - nh * 0.2,
            nx1 + nw * 1.2,
            ny1 + nh * 1.2,
        );
        for (px, py) in pts {
            total_pts += 1;
            if *px >= ax1 && *px <= ax2 && *py >= ay1 && *py <= ay2 {
                inside += 1;
            }
        }
        println!(
            "  框=({:.1},{:.1},{:.1},{:.1}) 置信度={:.3} kps=[{:.3},{:.3} {:.3},{:.3} {:.3},{:.3} {:.3},{:.3} {:.3},{:.3}]",
            x1 / 640.0, y1 / 640.0, (x2 - x1) / 640.0, (y2 - y1) / 640.0, s,
            pts[0].0, pts[0].1, pts[1].0, pts[1].1, pts[2].0, pts[2].1, pts[3].0, pts[3].1,
            pts[4].0, pts[4].1,
        );
    }
    println!(
        "kps 框内占比: {}/{}（{}%）",
        inside,
        total_pts,
        if total_pts == 0 { 0 } else { inside * 100 / total_pts }
    );
    if args.desc {
        let biggest = kept.iter().max_by(|a, b| {
            ((a.2 - a.0) * (a.3 - a.1)).partial_cmp(&((b.2 - b.0) * (b.3 - b.1))).unwrap_or(std::cmp::Ordering::Equal)
        });
        if let Some((x1, y1, x2, y2, _, pts)) = biggest {
            let (w, h) = ((x2 - x1) / 640.0, (y2 - y1) / 640.0);
            if w > 1e-4 && h > 1e-4 {
                let mut d = [0.0f32; 10];
                for (j, p) in pts.iter().enumerate() {
                    d[j * 2] = (p.0 - x1 / 640.0) / w;
                    d[j * 2 + 1] = (p.1 - y1 / 640.0) / h;
                }
                println!(
                    "desc {:.3} {:.3} {:.3} {:.3} {:.3} {:.3} {:.3} {:.3} {:.3} {:.3}",
                    d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7], d[8], d[9]
                );
            }
        }
    }
    Ok(())
}

fn iou5(a: &(f32, f32, f32, f32, f32, [(f32, f32); 5]), b: &(f32, f32, f32, f32, f32, [(f32, f32); 5])) -> f32 {
    let x0 = a.0.max(b.0);
    let y0 = a.1.max(b.1);
    let x1 = a.2.min(b.2);
    let y1 = a.3.min(b.3);
    let inter = (x1 - x0).max(0.0) * (y1 - y0).max(0.0);
    let ua = (a.2 - a.0) * (a.3 - a.1);
    let ub = (b.2 - b.0) * (b.3 - b.1);
    if ua + ub - inter <= 0.0 {
        0.0
    } else {
        inter / (ua + ub - inter)
    }
}
