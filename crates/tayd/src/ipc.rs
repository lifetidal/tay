//! daemon 侧 IPC:单实例互斥 + Unix socket 监听线程。
//!
//! 协议:换行分隔文本命令,应答 `ok`(命令已入队)/ `error <msg>`。
//! 命令:`toggle` / `show` / `hide` / `quit` / `pin`。
//!
//! 单实例(DESIGN §4.1):flock 锁文件为权威互斥,socket 为其载体。
//! 并发冷启动时(两个 CLI 同时拉起 daemon)只有一个能拿到锁;
//! 失败方静默退出(exit 0)。陈旧 socket(daemon 已死遗留)会被
//! 清理重建 —— 仅在持有锁时执行,不会删除活跃实例的 socket。
//!
//! 限制:单行 ≤ 4096 字节;并发处理线程 ≤ 16;读超时 5s。

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;
const MAX_LINE: usize = 4096;
const MAX_HANDLERS: usize = 16;
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// socket 路径:$XDG_RUNTIME_DIR/tay.sock(回退 /tmp/tay-{uid}.sock)。
pub fn socket_path() -> PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    dir.join("tay.sock")
}

fn lock_path() -> PathBuf {
    let mut p = socket_path();
    p.set_extension("lock");
    p
}

/// 单实例锁:成功返回持有句柄(进程生命周期内不得关闭)。
/// 失败 = 已有活跃实例(调用方应静默退出 0)。
pub fn acquire_lock() -> std::io::Result<std::fs::File> {
    acquire_lock_at(&lock_path())
}

/// 锁路径可注入(测试用临时目录,不碰真实 XDG_RUNTIME_DIR)。
fn acquire_lock_at(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    let ret = unsafe { libc::flock(file_fd(&file), libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        return Err(std::io::Error::new(
            err.kind(),
            format!("已有实例在运行(锁 {}): {err}", path.display()),
        ));
    }
    // 记录 PID 便于诊断。
    let _ = file.set_len(0);
    let _ = (&file).write_all(format!("{}\n", std::process::id()).as_bytes());
    Ok(file)
}

fn file_fd(f: &std::fs::File) -> std::os::fd::RawFd {
    use std::os::fd::AsRawFd;
    f.as_raw_fd()
}

/// 绑定监听 socket(调用方必须已持有单实例锁):
/// 活跃实例的 socket 不会被删除 —— 先 connect 探测,能连上即报错;
/// 只有连接失败的陈旧 socket 才清理重建。
pub fn bind() -> std::io::Result<UnixListener> {
    bind_at(&socket_path())
}

/// socket 路径可注入(测试用临时目录)。
fn bind_at(path: &std::path::Path) -> std::io::Result<UnixListener> {
    match UnixListener::bind(path) {
        Ok(l) => {
            log::info!("IPC socket: {}", path.display());
            Ok(l)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            match UnixStream::connect(path) {
                // 活跃实例在听:绝不删除。
                Ok(_) => Err(std::io::Error::other("daemon 已在运行(IPC socket 活跃)")),
                Err(_) => {
                    log::info!("清理陈旧 socket: {}", path.display());
                    std::fs::remove_file(path)?;
                    let l = UnixListener::bind(path)?;
                    log::info!("IPC socket: {}", path.display());
                    Ok(l)
                }
            }
        }
        Err(e) => Err(e),
    }
}

/// 命令通道。
pub fn channel() -> (Sender<IpcCommand>, Receiver<IpcCommand>) {
    std::sync::mpsc::channel::<IpcCommand>()
}

/// 启动 accept 线程(必须在 PTY fork 之后;fork 于多线程进程有死锁风险)。
/// `wake`:命令入队后向事件循环写 1 字节唤醒(非阻塞,失败可忽略——
/// 唤醒会被合并,下一轮事件自然处理)。
pub fn accept_loop(listener: UnixListener, tx: Sender<IpcCommand>, wake: Arc<UnixStream>) {
    let handlers = Arc::new(AtomicUsize::new(0));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            // 并发上限:防止连接风暴耗尽线程。
            if handlers.fetch_add(1, Ordering::AcqRel) >= MAX_HANDLERS {
                handlers.fetch_sub(1, Ordering::AcqRel);
                let _ = writeln!(&stream, "error 并发连接过多");
                continue;
            }
            let tx = tx.clone();
            let wake = wake.clone();
            let handlers = handlers.clone();
            std::thread::spawn(move || {
                handle_client(stream, tx, wake);
                handlers.fetch_sub(1, Ordering::AcqRel);
            });
        }
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpcCommand {
    Toggle,
    Show,
    Hide,
    Quit,
    /// 临时固定:暂停失焦自动隐藏(再发一次解除)。
    Pin,
    NewTab,
    NextTab,
    CloseTab,
    Settings,
    Maximize,
    Restore,
}

fn handle_client(stream: UnixStream, tx: Sender<IpcCommand>, wake: Arc<UnixStream>) {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let mut reader = BufReader::new(stream.try_clone().unwrap_or(stream));
    let mut line = String::new();
    loop {
        line.clear();
        // 行长度上限(超长 = 断开)。
        let mut limited = reader.by_ref().take(MAX_LINE as u64 + 1);
        match limited.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if line.len() > MAX_LINE {
            let _ = writeln!(reader.get_mut(), "error 行过长");
            break;
        }
        let cmd = line.trim();
        let parsed = match cmd {
            "toggle" => Some(IpcCommand::Toggle),
            "show" => Some(IpcCommand::Show),
            "hide" => Some(IpcCommand::Hide),
            "quit" => Some(IpcCommand::Quit),
            "pin" => Some(IpcCommand::Pin),
            "new-tab" => Some(IpcCommand::NewTab),
            "next-tab" => Some(IpcCommand::NextTab),
            "close-tab" => Some(IpcCommand::CloseTab),
            "settings" => Some(IpcCommand::Settings),
            "maximize" => Some(IpcCommand::Maximize),
            "restore" => Some(IpcCommand::Restore),
            "" => continue,
            other => {
                let _ = writeln!(reader.get_mut(), "error 未知命令: {other}");
                continue;
            }
        };
        if let Some(c) = parsed {
            // 语义:ok = 命令已入队(接收确认);操作完成状态见 daemon 日志。
            if tx.send(c).is_ok() {
                let _ = (&*wake).write(&[0]);
                let _ = writeln!(reader.get_mut(), "ok");
            } else {
                let _ = writeln!(reader.get_mut(), "error daemon 正在退出");
                break;
            }
        }
        if cmd == "quit" {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 临时目录(每测试独立子目录,避免并行测试互相删除;
    /// 绝不碰真实 XDG_RUNTIME_DIR / 运行中 socket)。
    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("tay-ipc-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn lock_is_exclusive() {
        let dir = temp_dir("exclusive");
        let path = dir.join("t.sock.lock");
        let l1 = acquire_lock_at(&path).expect("第一次加锁");
        let l2 = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("重开锁文件");
        let ret = unsafe { libc::flock(file_fd(&l2), libc::LOCK_EX | libc::LOCK_NB) };
        assert_ne!(ret, 0, "第二次加锁应失败(flock 互斥)");
        drop(l1);
        std::fs::remove_dir_all(dir).ok();
    }

    /// 并发加锁:两个句柄同时抢,只有一个成功(单实例语义)。
    #[test]
    fn concurrent_lock_only_one_wins() {
        let dir = temp_dir("concurrent");
        let path = dir.join("c.sock.lock");
        let l1 = acquire_lock_at(&path).expect("先到者成功");
        assert!(acquire_lock_at(&path).is_err(), "第二个持有者必须失败");
        drop(l1);
        std::fs::remove_dir_all(dir).ok();
    }

    /// 活跃 socket 不可被删:bind 到已监听的路径必须报错且保留原文件。
    #[test]
    fn active_socket_is_never_deleted() {
        let dir = temp_dir("active");
        let path = dir.join("live.sock");
        let _listener = UnixListener::bind(&path).expect("先绑定");
        assert!(bind_at(&path).is_err(), "活跃 socket 必须报错而非删除");
        assert!(path.exists(), "原 socket 文件必须保留");
        std::fs::remove_dir_all(dir).ok();
    }

    /// 陈旧 socket(无监听者)被清理重建。
    #[test]
    fn stale_socket_is_rebuilt() {
        let dir = temp_dir("stale");
        let path = dir.join("stale.sock");
        // 造一个陈旧路径:普通文件(非 socket),connect 必失败。
        std::fs::write(&path, b"stale").unwrap();
        let l = bind_at(&path).expect("陈旧 socket 应被清理重建");
        drop(l);
        assert!(path.exists());
        std::fs::remove_dir_all(dir).ok();
    }

    /// 全新路径直接绑定。
    #[test]
    fn fresh_socket_binds() {
        let dir = temp_dir("fresh");
        let path = dir.join("fresh.sock");
        let l = bind_at(&path).expect("全新路径绑定成功");
        drop(l);
        std::fs::remove_dir_all(dir).ok();
    }
}
