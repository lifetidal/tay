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

/// 终端流标记(命令块 / 图像)。
#[derive(Debug)]
pub enum BlockEvent {
    /// 提示符开始(网格行号)。
    PromptStart(i32),
    /// 上一条命令退出码。
    Exit { code: i32 },
    /// kitty graphics 图像(放置于流中光标处)。
    Image { img: crate::kitty::KittyImage, line: i32, col: u32 },
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
    let mut cell: (u32, u32) = (8, 16);
    let mut scanner = StreamScanner::default();
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

        eprintln!("[dbg] ptyloop iter");
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
                                for ev in scanner.scan(chunk) {
                                    match ev {
                                        StreamEvent::Text(bytes) => {
                                            let mut guard = term.lock();
                                            parser.advance(&mut *guard, &bytes);
                                        },
                                        StreamEvent::PromptStart => {
                                            let line = {
                                                let guard = term.lock();
                                                guard.renderable_content().cursor.point.line.0
                                            };
                                            let _ = blocks.send(BlockEvent::PromptStart(line));
                                        },
                                        StreamEvent::Exit(code) => {
                                            let _ = blocks.send(BlockEvent::Exit { code });
                                        },
                                        StreamEvent::Kitty(img) => {
                                            // 放置位置 = 当前光标(流序保证正确)。
                                            let (line, col) = {
                                                let guard = term.lock();
                                                let content = guard.renderable_content();
                                                (
                                                    content.cursor.point.line.0,
                                                    content.cursor.point.column.0 as u32,
                                                )
                                            };
                                            // 光标移动(kitty C=1 / sixel 下移,iTerm2 右移)。
                                            if !img.delete {
                                                let move_seq = match img.cursor_after {
                                                    crate::kitty::CursorAfter::None => None,
                                                    crate::kitty::CursorAfter::Down => {
                                                        let rows = (img.h as f32
                                                            / cell.1.max(1) as f32)
                                                            .ceil()
                                                            .max(1.0) as u32;
                                                        Some(format!("\r\x1b[{rows}B"))
                                                    },
                                                    crate::kitty::CursorAfter::Right => {
                                                        let cols = (img.w as f32
                                                            / cell.0.max(1) as f32)
                                                            .ceil()
                                                            .max(1.0) as u32;
                                                        Some(format!("\x1b[{cols}C"))
                                                    },
                                                };
                                                if let Some(seq) = move_seq {
                                                    let mut guard = term.lock();
                                                    parser.advance(&mut *guard, seq.as_bytes());
                                                }
                                            }
                                            let _ = blocks.send(BlockEvent::Image { img, line, col });
                                        },
                                    }
                                }
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
                                cell = (ws.cell_width as u32, ws.cell_height as u32);
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

/// 字节流事件(扫描器输出;term 无关,可单测)。
#[derive(Debug)]
enum StreamEvent {
    /// 喂给终端的字节。
    Text(Vec<u8>),
    /// kitty graphics 图像(已解码;放置位置由调用方按当前光标决定)。
    Kitty(crate::kitty::KittyImage),
    /// OSC 133 提示符开始。
    PromptStart,
    /// OSC 133 退出码。
    Exit(i32),
}

/// 流扫描器:字节 → 事件(OSC 133 命令块 + APC kitty graphics)。
#[derive(Default)]
struct StreamScanner {
    /// 待发文本缓冲。
    text: Vec<u8>,
    /// ESC ] 收集中(OSC)。
    osc: Vec<u8>,
    /// ESC _ 收集中(APC)。
    apc: Vec<u8>,
    /// ESC P 收集中(DCS/sixel)。
    dcs: Vec<u8>,
    /// StEsc 的来源(APC 或 DCS)。
    esc_origin: EscOrigin,
    state: ScanState,
}

#[derive(Default, Clone, Copy, PartialEq)]
enum EscOrigin {
    Apc,
    #[default]
    Dcs,
}

#[derive(Default, PartialEq)]
enum ScanState {
    #[default]
    Text,
    Osc,
    Apc,
    Dcs,
    /// ST(ESC \)的 ESC 已见。
    StEsc,
}

impl StreamScanner {
    fn scan(&mut self, chunk: &[u8]) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < chunk.len() {
            let c = chunk[i];
            match self.state {
                ScanState::Text => {
                    if c == 0x1b && i + 1 < chunk.len() {
                        match chunk[i + 1] {
                            b']' => {
                                self.state = ScanState::Osc;
                                self.osc.clear();
                                i += 2;
                                continue;
                            },
                            b'_' => {
                                self.state = ScanState::Apc;
                                self.apc.clear();
                                i += 2;
                                continue;
                            },
                            b'P' => {
                                self.state = ScanState::Dcs;
                                self.dcs.clear();
                                i += 2;
                                continue;
                            },
                            _ => {},
                        }
                    }
                    self.text.push(c);
                    i += 1;
                },
                ScanState::Osc => {
                    if c == 0x07 {
                        self.finish_osc(&mut out);
                        self.state = ScanState::Text;
                        i += 1;
                    } else if c == 0x1b && i + 1 < chunk.len() && chunk[i + 1] == b'\\' {
                        self.finish_osc(&mut out);
                        self.state = ScanState::Text;
                        i += 2;
                    } else {
                        self.osc.push(c);
                        i += 1;
                    }
                },
                ScanState::Apc => {
                    if c == 0x1b {
                        self.state = ScanState::StEsc;
                        self.esc_origin = EscOrigin::Apc;
                        i += 1;
                    } else {
                        self.apc.push(c);
                        i += 1;
                    }
                },
                ScanState::Dcs => {
                    if c == 0x1b {
                        self.state = ScanState::StEsc;
                        self.esc_origin = EscOrigin::Dcs;
                        i += 1;
                    } else {
                        self.dcs.push(c);
                        i += 1;
                    }
                },
                ScanState::StEsc => {
                    if c == b'\\' {
                        // APC / DCS 结束。
                        match self.esc_origin {
                            EscOrigin::Apc => self.finish_apc(&mut out),
                            EscOrigin::Dcs => self.finish_dcs(&mut out),
                        }
                        self.state = ScanState::Text;
                        i += 1;
                    } else {
                        // 不是 ST:ESC 属于内容。
                        match self.esc_origin {
                            EscOrigin::Apc => {
                                self.apc.push(0x1b);
                                self.apc.push(c);
                                self.state = ScanState::Apc;
                            },
                            EscOrigin::Dcs => {
                                self.dcs.push(0x1b);
                                self.dcs.push(c);
                                self.state = ScanState::Dcs;
                            },
                        }
                        i += 1;
                    }
                },
            }
        }
        // 防呆。
        if self.osc.len() > 512 || self.apc.len() > 64 * 1024 * 1024 || self.dcs.len() > 64 * 1024 * 1024 {
            self.state = ScanState::Text;
            self.osc.clear();
            self.apc.clear();
            self.dcs.clear();
        }
        self.flush_text(&mut out);
        out
    }

    fn finish_apc(&mut self, out: &mut Vec<StreamEvent>) {
        match crate::kitty::parse_apc(&self.apc) {
            crate::kitty::Apc::Kitty(img) => {
                self.flush_text(out);
                out.push(StreamEvent::Kitty(img));
            },
            crate::kitty::Apc::Passthrough(bytes) => {
                // 未知 APC:原样给终端(终端自行忽略)。
                self.text.extend_from_slice(b"\x1b_");
                self.text.extend_from_slice(&bytes);
                self.text.extend_from_slice(b"\x1b\\");
            },
            crate::kitty::Apc::Invalid => {},
        }
    }

    /// DCS:sixel 图像(其余 DCS 原样转发)。
    /// 判定靠解码成败:成功 = sixel,失败 = 别的 DCS(如 DECRQSS)。
    fn finish_dcs(&mut self, out: &mut Vec<StreamEvent>) {
        // 重建完整序列给 icy_sixel(参数可出现在 q 之前)。
        let mut seq = b"\x1bP".to_vec();
        seq.extend_from_slice(&self.dcs);
        seq.extend_from_slice(b"\x1b\\");
        if let Ok(img) = icy_sixel::SixelImage::decode(&seq) {
            let (w, h) = img.dimensions();
            if w > 0 && h > 0 && !img.pixels.is_empty() {
                self.flush_text(out);
                out.push(StreamEvent::Kitty(crate::kitty::KittyImage {
                    id: crate::kitty::fnv1a(&img.pixels) as u32,
                    w: w as u32,
                    h: h as u32,
                    cols: None,
                    rows: None,
                    rgba: img.pixels.clone(),
                    z: 0,
                    cursor_after: crate::kitty::CursorAfter::Down,
                    delete: false,
                    place_after: false,
                    target: None,
                }));
                return;
            }
        }
        // 非 sixel DCS:转发。
        self.text.extend_from_slice(b"\x1bP");
        self.text.extend_from_slice(&self.dcs);
        self.text.extend_from_slice(b"\x1b\\");
    }

    fn flush_text(&mut self, out: &mut Vec<StreamEvent>) {
        if !self.text.is_empty() {
            out.push(StreamEvent::Text(std::mem::take(&mut self.text)));
        }
    }

    fn finish_osc(&mut self, out: &mut Vec<StreamEvent>) {
        // OSC 原样转发给终端(终端处理标题/剪贴板等)。
        self.text.extend_from_slice(b"\x1b]");
        self.text.extend_from_slice(&self.osc);
        self.text.push(0x07);
        // 同时解析 133 命令块 / 1337 iTerm2 图像。
        if let Some(img) = crate::kitty::parse_iterm2(&self.osc) {
            self.flush_text(out);
            out.push(StreamEvent::Kitty(img));
        } else if let Some(m) = Self::parse_osc133(&self.osc) {
            self.flush_text(out);
            out.push(m);
        }
    }

    fn parse_osc133(buf: &[u8]) -> Option<StreamEvent> {
        let s = String::from_utf8_lossy(buf);
        let mut parts = s.split(';');
        if parts.next()? != "133" {
            return None;
        }
        match parts.next()? {
            "A" => Some(StreamEvent::PromptStart),
            "D" => Some(StreamEvent::Exit(parts.next()?.parse().ok().unwrap_or(0))),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_passthrough() {
        let mut sc = StreamScanner::default();
        let evs = sc.scan(b"hello");
        assert_eq!(evs.len(), 1);
        assert!(matches!(&evs[0], StreamEvent::Text(t) if t == b"hello"));
    }

    #[test]
    fn osc133_split_across_reads() {
        let mut sc = StreamScanner::default();
        assert!(sc.scan(b"\x1b]133;").is_empty());
        let evs = sc.scan(b"A\x07");
        // OSC 原样转发给终端 + 标记事件。
        assert!(matches!(&evs[0], StreamEvent::Text(_)));
        assert!(matches!(&evs[1], StreamEvent::PromptStart));
    }

    #[test]
    fn osc133_exit_code() {
        let mut sc = StreamScanner::default();
        let evs = sc.scan(b"\x1b]133;D;127\x07");
        assert!(matches!(&evs[0], StreamEvent::Text(_)));
        assert!(matches!(&evs[1], StreamEvent::Exit(127)));
    }

    #[test]
    fn kitty_image_apc() {
        let mut sc = StreamScanner::default();
        let mut seq = b"\x1b_Gf=32,s=1,v=1,i=5;".to_vec();
        seq.extend_from_slice(&[255, 0, 0, 255]);
        seq.extend_from_slice(b"\x1b\\");
        let evs = sc.scan(&seq);
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            StreamEvent::Kitty(img) => {
                assert_eq!(img.id, 5);
                assert_eq!(img.rgba, vec![255, 0, 0, 255]);
            },
            other => panic!("不是 kitty 事件: {other:?}"),
        }
    }

    #[test]
    fn kitty_apc_split_across_reads() {
        let mut sc = StreamScanner::default();
        assert!(sc.scan(b"before\x1b_Gf=32,s=1,v=1,i=1;").iter().all(|e| matches!(e, StreamEvent::Text(_))));
        let evs = sc.scan(b"\x00\x00\x00\xff\x1b\\after");
        assert!(evs.iter().any(|e| matches!(e, StreamEvent::Kitty(_))));
        assert!(evs.iter().any(|e| matches!(e, StreamEvent::Text(_))));
    }

    #[test]
    fn unknown_apc_passthrough() {
        let mut sc = StreamScanner::default();
        let evs = sc.scan(b"\x1b_Xother\x1b\\");
        assert_eq!(evs.len(), 1);
        assert!(matches!(&evs[0], StreamEvent::Text(t) if t == b"\x1b_Xother\x1b\\"));
    }

    #[test]
    fn sixel_dcs_decodes() {
        // 用 icy_sixel 编码 2×2 红色方块(输出自带完整 DCS 包装)。
        let rgba = [255u8, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255];
        let data = icy_sixel::sixel_encode_default(&rgba, 2, 2).expect("编码");
        let reference = icy_sixel::SixelImage::decode(data.as_bytes()).expect("参考解码");
        let mut sc = StreamScanner::default();
        let evs = sc.scan(data.as_bytes());
        assert!(evs.iter().any(|e| matches!(e, StreamEvent::Kitty(_))), "应有图像事件: {evs:?}");
        for ev in evs {
            if let StreamEvent::Kitty(img) = ev {
                assert_eq!((img.w as usize, img.h as usize), reference.dimensions());
                assert_eq!(img.rgba, reference.pixels);
            }
        }
    }

    #[test]
    fn non_sixel_dcs_passthrough() {
        let mut sc = StreamScanner::default();
        let evs = sc.scan(b"\x1bP1$r0 q\x1b\\");
        assert!(evs.iter().any(|e| matches!(e, StreamEvent::Text(t) if t.starts_with(b"\x1bP"))));
        assert!(!evs.iter().any(|e| matches!(e, StreamEvent::Kitty(_))));
    }

    #[test]
    fn st_esc_inside_apc() {
        // APC 内容中的孤立 ESC 并入内容;畸形 kitty 序列按协议丢弃。
        let mut sc = StreamScanner::default();
        assert!(sc.scan(b"\x1b_Gx\x1bz\x1b\\").is_empty());
    }
}
