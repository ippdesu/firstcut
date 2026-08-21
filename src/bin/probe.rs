//! 临时探测工具：打印 SCRFD ONNX 的输入输出元数据（M5 换模型用）
//! 用法: pic_process-probe <onnx路径> [--size 640]

use anyhow::Result;
use clap::Parser;
use ndarray::Array4;
use ort::session::Session;

#[derive(Parser)]
struct Args {
    path: std::path::PathBuf,
    #[arg(long, default_value_t = 640)]
    size: usize,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mut builder = Session::builder().map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let mut session = builder
        .commit_from_file(&args.path)
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;

    // 构造输入跑一次，打印所有输出形状
    let arr = Array4::<f32>::zeros((1, 3, args.size, args.size));
    let tensor = ort::value::Tensor::from_array(arr).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let input = ort::inputs!["input" => tensor];
    let first_err = match session.run(input) {
        Ok(out) => {
            for (i, o) in out.iter().enumerate() {
                if let Ok((shape, _)) = o.1.try_extract_tensor::<f32>() {
                    println!("output[{i}] 名称={} 形状={:?}", o.0, shape.to_vec());
                }
            }
            return Ok(());
        }
        Err(e) => format!("{e:?}"),
    };

    // 输入名可能不是 "input"：用常见候选重试
    for name in ["images", "img", "data", "input.1"] {
        let arr = Array4::<f32>::zeros((1, 3, args.size, args.size));
        let t = ort::value::Tensor::from_array(arr).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let input = ort::inputs![name => t];
        if let Ok(out) = session.run(input) {
            println!("输入名 = {name}");
            for (i, o) in out.iter().enumerate() {
                if let Ok((shape, _)) = o.1.try_extract_tensor::<f32>() {
                    println!("output[{i}] 名称={} 形状={:?}", o.0, shape.to_vec());
                }
            }
            return Ok(());
        }
    }
    anyhow::bail!("输入名探测失败: {first_err}")
}
