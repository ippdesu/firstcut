#![cfg_attr(windows, windows_subsystem = "windows")]

use std::path::PathBuf;

#[cfg(windows)]
fn main() {
    if already_running() {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", "http://127.0.0.1:8787/"])
            .spawn();
        return;
    }

    let root = std::env::args_os().nth(1).map(PathBuf::from).or_else(|| {
        rfd::FileDialog::new()
            .set_title("选择照片目录（若 JPG 和 RAW 分开放置，请选择同时包含两者的上级目录）")
            .pick_folder()
    });

    let Some(root) = root else {
        return;
    };

    if let Err(error) = run_review(&root) {
        rfd::MessageDialog::new()
            .set_title("firstcut 启动失败")
            .set_description(format!("无法启动照片复核界面：\n\n{error:#}"))
            .set_level(rfd::MessageLevel::Error)
            .show();
    }
}

/// 浏览器标签页被关掉时，再次双击启动器可找回仍在运行的复核页面。
#[cfg(windows)]
fn already_running() -> bool {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};
    use std::time::Duration;

    let address: SocketAddr = "127.0.0.1:8787".parse().unwrap();
    let Ok(mut stream) = TcpStream::connect_timeout(&address, Duration::from_secs(1)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    if stream.write_all(b"GET /api/control HTTP/1.1\r\nHost: 127.0.0.1:8787\r\nConnection: close\r\n\r\n").is_err() {
        return false;
    }
    let mut response = String::new();
    stream.read_to_string(&mut response).is_ok() && response.contains("\"enabled\":true")
}

#[cfg(windows)]
fn run_review(root: &std::path::Path) -> anyhow::Result<()> {
    anyhow::ensure!(root.is_dir(), "选择的路径不是目录：{}", root.display());
    let root = pic_process::review::normal_photo_root(root)?;
    use_model_directory_beside_executable()?;

    pic_process::review::serve_ui(&root, || {
        rfd::FileDialog::new()
            .set_title("选择已评分的照片目录")
            .pick_folder()
    })?;
    Ok(())
}

#[cfg(windows)]
fn use_model_directory_beside_executable() -> anyhow::Result<()> {
    if pic_process::ai::ensure_models().is_ok() {
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    if let Some(parent) = exe.parent() {
        for dir in parent.ancestors() {
            if ["clipiqa_model.onnx", "clipiqa_model.onnx.data",
                "scrfd_10g_bnkps.onnx", "yolov8n_pose.onnx"]
                .iter().all(|name| dir.join("models").join(name).is_file())
            {
                std::env::set_current_dir(dir)?;
                return Ok(());
            }
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("firstcut-ui is currently available only in Windows releases.");
}
