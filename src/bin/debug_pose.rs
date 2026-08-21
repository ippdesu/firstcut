//! 临时诊断工具：YOLOv8-pose 输出检查（M5 小脸人像排查）
//! 用法: pic_process-debug-pose <JPG路径>

use anyhow::Result;
use clap::Parser;
use pic_process::ai::pose::{PoseDet, head_region};
use pic_process::decode;

#[derive(Parser)]
struct Args {
    path: std::path::PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let img = decode::load_analysis_image(&args.path)?.expect("解码失败");
    let mut pose = PoseDet::load(1)?;
    let persons = pose.detect(&img.rgb, img.width, img.height)?;
    println!("检测到 {} 个人体", persons.len());
    for (i, p) in persons.iter().enumerate() {
        println!(
            "  [{i}] 人体框=({:.3},{:.3},{:.3},{:.3}) 置信度={:.3}",
            p.x, p.y, p.w, p.h, p.score
        );
        let names = ["鼻子", "左眼", "右眼", "左耳", "右耳", "左肩", "右肩"];
        for (k, name) in names.iter().enumerate() {
            let (kx, ky, kc) = p.keypoints[k];
            println!("     {name}: ({kx:.3},{ky:.3}) conf={kc:.3}");
        }
        match head_region(p) {
            Some((cx, cy, hw, hh)) => {
                println!("     头部区域: 中心=({cx:.3},{cy:.3}) 半宽={hw:.3} 半高={hh:.3}");
                let reblur = pic_process::metrics::sharpness::reblur_mean_region(
                    &img.luma, img.width, img.height, cx, cy, hw, hh,
                );
                println!(
                    "     头部区域 reblur={reblur:.2} → 区域锐度分={:.1}",
                    pic_process::metrics::sharpness::region_sharpness_score(reblur)
                );
            }
            None => println!("     头部区域: 无（关键点不足）"),
        }
    }
    Ok(())
}
