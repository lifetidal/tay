//! tay — Tay CLI 薄客户端(DESIGN §4.1)。
//!
//! `tay [toggle|show|hide|quit]`(默认 toggle):连 daemon 发命令即退出。
//! daemon 不在时自动拉起(cold start 降级路径,§4.3)。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

fn socket_path() -> PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    dir.join("tay.sock")
}

fn send(cmd: &str) -> Result<String, String> {
    let mut stream =
        UnixStream::connect(socket_path()).map_err(|_| "daemon 未运行".to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .map_err(|e| e.to_string())?;
    writeln!(stream, "{cmd}").map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    Ok(line.trim().to_string())
}

/// 拉起 daemon(tayd 需在 PATH 或与 tay 同目录)。
fn spawn_daemon() {
    let exe = std::env::current_exe().ok();
    let candidates: Vec<&str> = if let Some(exe) = &exe {
        if let Some(dir) = exe.parent() {
            let p = dir.join("tayd");
            if p.exists() {
                return spawn(&p);
            }
        }
        vec!["tayd"]
    } else {
        vec!["tayd"]
    };
    for c in candidates {
        if let Ok(out) = Command::new(c).arg("--daemon").spawn() {
            let _ = out;
            return;
        }
    }
    eprintln!("tay: 找不到 tayd,无法拉起 daemon");
    std::process::exit(1);
}

fn spawn(path: &std::path::Path) {
    let _ = Command::new(path).spawn();
}

fn main() {
    let cmd = std::env::args().nth(1).unwrap_or_else(|| "toggle".into());
    let cmd = match cmd.as_str() {
        "toggle" | "show" | "hide" | "quit" => cmd,
        "--daemon" => return, // 被 CLI 拉起时:正常初始化 daemon
        _ => {
            eprintln!("用法: tay [toggle|show|hide|quit](默认 toggle)");
            std::process::exit(2);
        },
    };

    match send(&cmd) {
        Ok(resp) => {
            if resp != "ok" {
                eprintln!("tay: {resp}");
                std::process::exit(1);
            }
        },
        Err(_) => {
            // 冷启动:拉起 daemon 后重试(§4.3 降级路径)。
            spawn_daemon();
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                std::thread::sleep(Duration::from_millis(50));
                if let Ok(resp) = send(&cmd) {
                    if resp != "ok" {
                        eprintln!("tay: {resp}");
                        std::process::exit(1);
                    }
                    return;
                }
                if Instant::now() > deadline {
                    eprintln!("tay: daemon 启动超时");
                    std::process::exit(1);
                }
            }
        },
    }
}
