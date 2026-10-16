//! daemon 侧 IPC:Unix socket 监听线程。
//!
//! 协议(M1):换行分隔文本命令,应答 `ok` / `error <msg>`。
//! 命令:`toggle` / `show` / `hide` / `quit`。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};

/// socket 路径:$XDG_RUNTIME_DIR/tay.sock(回退 /tmp/tay-{uid}.sock)。
pub fn socket_path() -> PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    dir.join("tay.sock")
}

/// 绑定 IPC socket(主线程调用,无 fork,失败 = 已有实例)。
pub fn bind() -> std::io::Result<UnixListener> {
    let path = socket_path();
    // 清掉陈旧 socket(上次异常退出遗留)。
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }
    let listener = UnixListener::bind(&path)?;
    log::info!("IPC socket: {}", path.display());
    Ok(listener)
}

/// 启动 accept 线程(必须在 PTY fork 之后;fork 于多线程进程有死锁风险)。
pub fn accept_loop(listener: UnixListener, tx: Sender<IpcCommand>, wake: Sender<()>) {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let tx = tx.clone();
            let wake = wake.clone();
            std::thread::spawn(move || handle_client(stream, tx, wake));
        }
    });
}

/// 命令通道。
pub fn channel() -> (Sender<IpcCommand>, Receiver<IpcCommand>) {
    std::sync::mpsc::channel::<IpcCommand>()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpcCommand {
    Toggle,
    Show,
    Hide,
    Quit,
}

fn handle_client(stream: UnixStream, tx: Sender<IpcCommand>, wake: Sender<()>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap_or(stream));
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {},
        }
        let cmd = line.trim();
        let parsed = match cmd {
            "toggle" => Some(IpcCommand::Toggle),
            "show" => Some(IpcCommand::Show),
            "hide" => Some(IpcCommand::Hide),
            "quit" => Some(IpcCommand::Quit),
            "" => continue,
            other => {
                let _ = writeln!(reader.get_mut(), "error unknown command: {other}");
                continue;
            },
        };
        if let Some(c) = parsed {
            if tx.send(c).is_ok() {
                let _ = wake.send(());
                let _ = writeln!(reader.get_mut(), "ok");
            }
        }
        if cmd == "quit" {
            break;
        }
    }
}
