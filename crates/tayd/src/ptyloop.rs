//! 自研 PTY 事件循环:替换 alacritty_terminal::event_loop::EventLoop。
//!
//! 为什么自研:alacritty 的事件循环把 PTY 读到的字节直接喂给 vte parser,
//! 前端看不到输出流 —— 而 OSC 133 命令块(DESIGN §8.4)需要前端消费。
//! 自研后读取路径归我们,顺手解析 OSC 133,其余行为与原版一致
//! (poll PTY + 唤醒管道、DEC 2026 同步超时、子进程退出通知)。

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener, OnResize, WindowSize};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use alacritty_terminal::tty::{self, ChildEvent, EventedPty};
use alacritty_terminal::vte::ansi;
use polling::{Event as PollEvent, Poller};

/// OSC 133 标记(命令块)。
#[derive(Clone, Copy, Debug)]
pub enum BlockEvent {
    /// 提示符开始(网格行号)。
    PromptStart(i32),
    /// 上一条命令退出码。
    Exit { code: i32 },
}

/// 给 UI 线程的发送端。
#[derive(Clone)]
pub struct TermSender {
    tx: Sender<Msg>,
    wake: Arc<UnixStream>,
}

#[derive(Debug)]
pub enum Msg {
    Input(Vec<u8>),
    Resize(WindowSize),
    Shutdown,
}

impl TermSender {
    pub fn send(&self, msg: Msg) -> bool {
        if self.tx.send(msg).is_err() {
            return false;
        }
        // 唤醒 poll 循环。
        let _ = (&*self.wake).write(&[0]);
        true
    }
}

/// 拉起事件循环线程。
pub fn spawn<U>(
    term: Arc<FairMutex<Term<U>>>,
    listener: U,
    pty: tty::Pty,
    blocks: Sender<BlockEvent>,
) -> std::io::Result<TermSender>
where
    U: EventListener + Send + 'static,
{
    let (tx, rx) = mpsc::channel::<Msg>();
    let (wake_tx, wake_rx) = UnixStream::pair()?;
    wake_tx.set_nonblocking(true)?;
    wake_rx.set_nonblocking(true)?;

    std::thread::Builder::new()
        .name("pty".into())
        .spawn(move || run(term, listener, pty, blocks, rx, wake_rx))
        .map_err(|e| std::io::Error::other(e))?;

    Ok(TermSender { tx, wake: Arc::new(wake_tx) })
}

const PTY_KEY: usize = 1;
const WAKE_KEY: usize = 2;

fn run<U>(
    term: Arc<FairMutex<Term<U>>>,
    listener: U,
    mut pty: tty::Pty,
    blocks: Sender<BlockEvent>,
    rx: Receiver<Msg>,
    mut wake_rx: UnixStream,
) where
    U: EventListener,
{
    let poller = match Poller::new() {
        Ok(p) => p,
        Err(e) => {
            log::error!("PTY poller 创建失败: {e}");
            return;
        },
    };
    // PTY 读取源(与原版一样注册 master fd)。
    unsafe {
        if poller
            .add_with_mode(pty.file(), PollEvent::readable(PTY_KEY), polling::PollMode::Level)
            .is_err()
        {
            log::error!("PTY fd 注册失败");
            return;
        }
        if poller
            .add_with_mode(&wake_rx, PollEvent::readable(WAKE_KEY), polling::PollMode::Level)
            .is_err()
        {
            log::error!("唤醒管道注册失败");
            return;
        }
    }

    let mut parser: ansi::Processor = ansi::Processor::new();
    let mut osc = OscState::default();
    let mut buf = vec![0u8; 16 * 1024];
    let mut events = polling::Events::new();
    let mut exiting = false;

    loop {
        // DEC 2026 同步超时:即便无数据也要按时推进 parser(与原版同款)。
        let handler = parser.sync_timeout();
        let timeout = handler
            .sync_timeout()
            .map(|st| st.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::from_millis(500))
            .min(Duration::from_millis(500));

        events.clear();
        if let Err(e) = poller.wait(&mut events, Some(timeout)) {
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            log::error!("PTY poll 失败: {e}");
            break;
        }

        for ev in events.iter() {
            match ev.key {
                PTY_KEY => {
                    let mut read_any = false;
                    loop {
                        match pty.file().read(&mut buf) {
                            Ok(0) => {
                                exiting = true;
                                break;
                            },
                            Ok(n) => {
                                read_any = true;
                                let chunk = &buf[..n];
                                // OSC 133 消费(前端命令块)。
                                osc.scan_into(chunk, &term, &blocks);
                                // 喂给终端内核。
                                let mut guard = term.lock();
                                parser.advance(&mut *guard, chunk);
                            },
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                            Err(e) => {
                                log::error!("PTY 读取失败: {e}");
                                exiting = true;
                                break;
                            },
                        }
                    }
                    let _ = read_any;
                },
                WAKE_KEY => {
                    // 排空唤醒字节,处理命令。
                    let mut sink = [0u8; 64];
                    while wake_rx.read(&mut sink).is_ok() {}
                    while let Ok(msg) = rx.try_recv() {
                        match msg {
                            Msg::Input(bytes) => {
                                let _ = pty.file().write_all(&bytes);
                            },
                            Msg::Resize(ws) => {
                                pty.on_resize(ws);
                            },
                            Msg::Shutdown => {
                                exiting = true;
                            },
                        }
                    }
                },
                _ => {},
            }
        }

        // 同步超时推进(无数据但到期;原版语义)。
        if events.is_empty() && rx.try_recv().is_err() {
            parser.stop_sync(&mut *term.lock());
            listener.send_event(Event::Wakeup);
        }

        // 子进程退出。
        if let Some(ChildEvent::Exited(status)) = pty.next_child_event() {
            if let Some(status) = status {
                listener.send_event(Event::ChildExit(status));
            }
            break;
        }
        if exiting {
            break;
        }
    }
    log::debug!("PTY 事件循环退出");
}

/// OSC 133 原始标记(字节级解析产物,与 term 无关,可单测)。
#[derive(Debug, PartialEq, Eq)]
enum RawMarker {
    PromptStart,
    Exit(i32),
}

/// OSC 133 解析状态机(跨 read 分块)。
#[derive(Default)]
struct OscState {
    /// 正在收的 OSC 内容(ESC ] 之后的部分)。
    buf: Vec<u8>,
    active: bool,
}

impl OscState {
    /// 字节级扫描:返回本块内完成的标记。
    fn scan(&mut self, chunk: &[u8]) -> Vec<RawMarker> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < chunk.len() {
            if !self.active {
                // 找 ESC ] 开头。
                if chunk[i] == 0x1b && i + 1 < chunk.len() && chunk[i + 1] == b']' {
                    self.active = true;
                    self.buf.clear();
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
            // OSC 内:找 BEL 或 ST(ESC \)。
            let c = chunk[i];
            if c == 0x07 || (c == 0x1b && i + 1 < chunk.len() && chunk[i + 1] == b'\\') {
                if let Some(m) = Self::parse(&self.buf) {
                    out.push(m);
                }
                self.active = false;
                self.buf.clear();
                i += if c == 0x07 { 1 } else { 2 };
            } else {
                self.buf.push(c);
                i += 1;
            }
        }
        // 防呆:异常内容丢弃。
        if self.buf.len() > 512 {
            self.active = false;
            self.buf.clear();
        }
        out
    }

    fn parse(buf: &[u8]) -> Option<RawMarker> {
        let s = String::from_utf8_lossy(buf);
        let mut parts = s.split(';');
        if parts.next()? != "133" {
            return None;
        }
        match parts.next()? {
            "A" => Some(RawMarker::PromptStart),
            "D" => Some(RawMarker::Exit(parts.next()?.parse().ok().unwrap_or(0))),
            _ => None,
        }
    }

    fn scan_into<U>(
        &mut self,
        chunk: &[u8],
        term: &Arc<FairMutex<Term<U>>>,
        blocks: &Sender<BlockEvent>,
    ) where
        U: EventListener,
    {
        for m in self.scan(chunk) {
            match m {
                // A = 提示符开始:记录当前光标所在网格行。
                RawMarker::PromptStart => {
                    let line = term.lock().renderable_content().cursor.point.line.0;
                    log::debug!("OSC133 A @ line {line}");
                    let _ = blocks.send(BlockEvent::PromptStart(line));
                },
                // D = 退出码。
                RawMarker::Exit(code) => {
                    log::debug!("OSC133 D code={code}");
                    let _ = blocks.send(BlockEvent::Exit { code });
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple() {
        let mut st = OscState::default();
        let m = st.scan(b"\x1b]133;A\x07");
        assert_eq!(m, vec![RawMarker::PromptStart]);
        let m = st.scan(b"\x1b]133;D;127\x07");
        assert_eq!(m, vec![RawMarker::Exit(127)]);
    }

    #[test]
    fn parse_split_across_reads() {
        let mut st = OscState::default();
        assert!(st.scan(b"\x1b]133;").is_empty());
        assert_eq!(st.scan(b"A\x07"), vec![RawMarker::PromptStart]);
    }

    #[test]
    fn parse_st_terminated() {
        let mut st = OscState::default();
        assert_eq!(st.scan(b"\x1b]133;D;3\x1b\\"), vec![RawMarker::Exit(3)]);
    }

    #[test]
    fn ignores_other_osc() {
        let mut st = OscState::default();
        assert!(st.scan(b"\x1b]0;title\x07").is_empty());
        assert!(st.scan(b"\x1b]52;c;Zm9v\x07").is_empty());
    }
}
