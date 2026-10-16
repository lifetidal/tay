//! TayApp:终端内核 + PTY + 窗口 + 渲染 + 呼出状态机(M1)。
//!
//! 数据流:
//! - 键盘/IME → `Msg::Input` → PTY → shell → 输出 → term 解析 → `Wakeup` → 重绘
//! - 呼出/收起 = layer margin 动画(DESIGN §8.1):GPU surface 常驻,零重建
//! - 热键(portal)/ IPC(`tay toggle`)/ 失焦(keyboard leave)三者汇入同一状态机

use std::sync::{mpsc, Arc, Mutex};
use std::time::Instant;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::Scroll;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors as TermColors;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::tty::{self, Options as PtyOptions, Shell};
use alacritty_terminal::vte::ansi::{Color, CursorShape as ACursorShape, NamedColor};

use crate::ptyloop::{BlockEvent, Msg, TermSender};
use tay_config::{Config, Rgb};
use tay_render::wgpu;
use tay_window::{
    Ctx, Event as WEvent, Handler, ImeEvent, KeyKind, MouseButtonIdx,
};

/// xterm 标准 16 色(term 调色板为空时的默认)。
const ANSI16: [Rgb; 16] = [
    Rgb::new(0x00, 0x00, 0x00),
    Rgb::new(0xcd, 0x00, 0x00),
    Rgb::new(0x00, 0xcd, 0x00),
    Rgb::new(0xcd, 0xcd, 0x00),
    Rgb::new(0x00, 0x00, 0xee),
    Rgb::new(0xcd, 0x00, 0xcd),
    Rgb::new(0x00, 0xcd, 0xcd),
    Rgb::new(0xe5, 0xe5, 0xe5),
    Rgb::new(0x7f, 0x7f, 0x7f),
    Rgb::new(0xff, 0x00, 0x00),
    Rgb::new(0x00, 0xff, 0x00),
    Rgb::new(0xff, 0xff, 0x00),
    Rgb::new(0x5c, 0x5c, 0xff),
    Rgb::new(0xff, 0x00, 0xff),
    Rgb::new(0x00, 0xff, 0xff),
    Rgb::new(0xff, 0xff, 0xff),
];

fn dim(c: Rgb) -> Rgb {
    Rgb::new(c.0 / 2, c.1 / 2, c.2 / 2)
}

/// alacritty_terminal 要求的网格尺寸(其 (usize,usize) 实现仅测试可见)。
struct Dims {
    cols: usize,
    rows: usize,
}

impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// 解析单元格颜色:term 调色板(OSC 4 可覆盖)→ 标准 16 色 / 配置色。
fn resolve(c: Color, colors: &TermColors, cfg: &Config) -> Rgb {
    let from_palette = |i: usize, fallback: Rgb| -> Rgb {
        colors[i].map(|r| Rgb::new(r.r, r.g, r.b)).unwrap_or(fallback)
    };
    match c {
        Color::Named(n) => match n {
            NamedColor::Foreground | NamedColor::BrightForeground => cfg.colors.foreground,
            NamedColor::Background => cfg.colors.background,
            NamedColor::Cursor => cfg.colors.cursor,
            NamedColor::DimForeground => dim(cfg.colors.foreground),
            NamedColor::DimBlack
            | NamedColor::DimRed
            | NamedColor::DimGreen
            | NamedColor::DimYellow
            | NamedColor::DimBlue
            | NamedColor::DimMagenta
            | NamedColor::DimCyan
            | NamedColor::DimWhite => {
                let base = n as usize - NamedColor::DimBlack as usize;
                from_palette(n as usize, dim(ANSI16[base]))
            },
            _ => from_palette(n as usize, ANSI16[n as usize]),
        },
        Color::Spec(r) => Rgb::new(r.r, r.g, r.b),
        Color::Indexed(i) => from_palette(16 + i as usize, ANSI16[i as usize % 16]),
    }
}

/// 终端事件转发:PTY 线程 → UI 线程。
#[derive(Clone)]
struct Listener {
    tx: mpsc::Sender<Event>,
    wake: mpsc::Sender<()>,
    sender: Arc<Mutex<Option<TermSender>>>,
}

impl Listener {
    fn new(tx: mpsc::Sender<Event>, wake: mpsc::Sender<()>) -> Self {
        Self { tx, wake, sender: Arc::new(Mutex::new(None)) }
    }
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        match event {
            // term 要写给 PTY 的字节(对查询序列的应答等)。
            Event::PtyWrite(text) => {
                if let Some(s) = self.sender.lock().unwrap().as_ref() {
                    let _ = s.send(Msg::Input(text.into_bytes().into()));
                }
            },
            event => {
                let _ = self.tx.send(event);
                let _ = self.wake.send(());
            },
        }
    }
}

/// 呼出/收起状态机:layer margin 动画。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Vis {
    Hidden,
    Shown,
    Animating { from: i32, to: i32, start: Instant, target: bool },
}

pub struct TayApp {
    config: Config,
    shot: Option<(String, u32)>,
    bench: Option<u32>,

    instance: Option<wgpu::Instance>,
    renderer: Option<tay_render::Renderer>,
    term: Option<Arc<FairMutex<Term<Listener>>>>,
    sender: Option<TermSender>,
    tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
    blocks_tx: mpsc::Sender<BlockEvent>,
    blocks_rx: mpsc::Receiver<BlockEvent>,
    /// 命令块(最新在尾):prompt 网格行号 + 退出码。
    blocks: Vec<(i32, Option<i32>)>,

    dims: Option<(usize, usize)>,
    preedit: Option<tay_render::Preedit>,
    /// 鼠标选区(显示坐标):(起点, 当前点)。
    sel: Option<((usize, usize), (usize, usize))>,
    dragging: bool,
    scroll_px: f32,

    // —— M1:呼出状态 ——
    vis: Vis,
    // —— M3:QuickSelect ——
    quick: Option<QuickState>,
    /// 等待 set_size 后的 configure(避免 wgpu swapchain 二次重建)。
    sizing: bool,
    layer_h: i32,
    shown_once: bool,
    show_at: Option<Instant>,
    ipc_listener: Option<std::os::unix::net::UnixListener>,
    ipc_tx: mpsc::Sender<crate::ipc::IpcCommand>,
    ipc_rx: mpsc::Receiver<crate::ipc::IpcCommand>,
    hotkey_tx: mpsc::Sender<crate::hotkey::HotkeyTriggered>,
    hotkey_rx: mpsc::Receiver<crate::hotkey::HotkeyTriggered>,
    wake: mpsc::Sender<()>,
    /// init 完成后启动的后台线程(PTY fork 之后才能起)。
    threads_started: bool,

    frames: u32,
    bench_started: bool,
    bench_times: Vec<f32>,
    exec: Option<String>,
    exec_sent: bool,
    quickselect_at_start: bool,
}

fn ease_out_cubic(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

struct QuickState {
    matches: Vec<crate::quickselect::Match>,
    typed: String,
}

impl QuickState {
    /// 已输入前缀能唯一指向的标签。
    fn resolve(&self) -> Option<&crate::quickselect::Match> {
        self.matches.iter().find(|m| m.label == self.typed)
    }
}

impl TayApp {
    pub fn new(
        config: Config,
        shot: Option<(String, u32)>,
        bench: Option<u32>,
        exec: Option<String>,
        quickselect_at_start: bool,
        ipc_listener: Option<std::os::unix::net::UnixListener>,
        ipc_tx: mpsc::Sender<crate::ipc::IpcCommand>,
        ipc_rx: mpsc::Receiver<crate::ipc::IpcCommand>,
        hotkey_tx: mpsc::Sender<crate::hotkey::HotkeyTriggered>,
        hotkey_rx: mpsc::Receiver<crate::hotkey::HotkeyTriggered>,
        wake: mpsc::Sender<()>,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let (blocks_tx, blocks_rx) = mpsc::channel();
        Self {
            config,
            shot,
            bench,
            instance: None,
            renderer: None,
            term: None,
            sender: None,
            tx,
            rx,
            blocks_tx,
            blocks_rx,
            blocks: Vec::new(),
            dims: None,
            preedit: None,
            sel: None,
            dragging: false,
            scroll_px: 0.0,
            vis: Vis::Hidden,
            quick: None,
            sizing: false,
            layer_h: 480,
            shown_once: false,
            show_at: None,
            ipc_listener,
            ipc_tx,
            ipc_rx,
            hotkey_tx,
            hotkey_rx,
            wake,
            threads_started: false,
            frames: 0,
            bench_started: false,
            bench_times: Vec::new(),
            exec,
            exec_sent: false,
            quickselect_at_start,
        }
    }

    /// 呼出(margin 动画到 0)。
    fn show(&mut self, ctx: &mut Ctx<'_>) {
        if self.vis == Vis::Shown {
            return;
        }
        let from = self.current_margin();
        self.show_at = Some(Instant::now());
        self.vis = Vis::Animating { from, to: 0, start: Instant::now(), target: true };
        // 呼出即渲染首帧(内容可能已过时;同时完成热呼出延迟测量)。
        ctx.request_redraw();
    }

    /// 收起(margin 动画到 -h)。
    fn hide(&mut self, _ctx: &mut Ctx<'_>) {
        if self.vis == Vis::Hidden {
            return;
        }
        let from = self.current_margin();
        self.vis = Vis::Animating { from, to: -self.layer_h, start: Instant::now(), target: false };
    }

    /// toggle:动画中则反向。
    fn toggle(&mut self, ctx: &mut Ctx<'_>) {
        match self.vis {
            Vis::Hidden => self.show(ctx),
            Vis::Shown => self.hide(ctx),
            Vis::Animating { target, .. } => {
                if target {
                    self.hide(ctx);
                } else {
                    self.show(ctx);
                }
            },
        }
    }

    fn current_margin(&self) -> i32 {
        match self.vis {
            Vis::Hidden => -self.layer_h,
            Vis::Shown => 0,
            // 动画中:近似取 from(下一步 tick 会重算)。
            Vis::Animating { from, .. } => from,
        }
    }

    /// 动画步进(AboutToWait 驱动,每 ~5ms 一 tick)。
    fn step_animation(&mut self, ctx: &mut Ctx<'_>) {
        let Vis::Animating { from, to, start, target } = self.vis else { return };
        let ms = self.config.animation_ms.max(1) as f32;
        let t = (start.elapsed().as_secs_f32() * 1000.0 / ms).clamp(0.0, 1.0);
        let margin = from + ((to - from) as f32 * ease_out_cubic(t)).round() as i32;
        if margin != self.current_margin() {
            ctx.set_top_margin(margin);
        }
        if t >= 1.0 {
            self.vis = if target { Vis::Shown } else { Vis::Hidden };
        }
    }

    /// can_create_surfaces:建 GPU → 终端 → PTY 线程 → 定尺寸。
    fn init(&mut self, ctx: &mut Ctx<'_>) {
        let result = (|| -> Result<(), String> {
            // layer 高度 = 输出逻辑高度 × height_percent。
            let (out_w, out_h) = ctx
                .output_logical_size()
                .unwrap_or((1920, 1080));
            let h = (out_h * self.config.height_percent / 100).max(200);
            self.layer_h = h as i32;
            log::info!("layer 高度 {h}px(输出 {out_w}×{out_h})");
            ctx.set_size(0, h);

            let (disp_ptr, surf_ptr) = ctx.raw_wayland().ok_or("取 Wayland 句柄失败")?;
            let mut instance_desc = wgpu::InstanceDescriptor::new_without_display_handle();
            instance_desc.backends = wgpu::Backends::PRIMARY | wgpu::Backends::GL;
            let instance = wgpu::Instance::new(instance_desc);
            let raw_display =
                wgpu::rwh::RawDisplayHandle::Wayland(wgpu::rwh::WaylandDisplayHandle::new(
                    std::ptr::NonNull::new(disp_ptr as *mut _).expect("display 指针非空"),
                ));
            let raw_window =
                wgpu::rwh::RawWindowHandle::Wayland(wgpu::rwh::WaylandWindowHandle::new(
                    std::ptr::NonNull::new(surf_ptr as *mut _).expect("surface 指针非空"),
                ));
            let surface = unsafe {
                instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                    raw_display_handle: Some(raw_display),
                    raw_window_handle: raw_window,
                })
            }
            .map_err(|e| format!("创建 surface 失败: {e}"))?;

            let size = ctx.surface_size();
            let shot = self.shot.as_ref().map(|(p, n)| (p.as_str(), *n));
            let renderer = tay_render::Renderer::new(
                &instance,
                surface,
                &self.config,
                size,
                ctx.scale_factor(),
                shot,
            )?;
            let (cw, ch) = renderer.cell_size();
            let pad = self.config.padding;
            let cols = ((size.0 as f32 - 2.0 * pad) / cw).floor().max(2.0) as usize;
            let rows = ((size.1 as f32 - 2.0 * pad) / ch).floor().max(1.0) as usize;
            log::info!("网格 {cols}×{rows} @ {cw:.1}×{ch:.1}px 单元格");
            self.dims = Some((cols, rows));
            self.renderer = Some(renderer);
            self.instance = Some(instance);

            // 终端 + PTY 事件循环线程。
            let listener = Listener::new(self.tx.clone(), self.wake.clone());
            let term_config = TermConfig { scrolling_history: 10_000, ..Default::default() };
            let term = Arc::new(FairMutex::new(Term::new(
                term_config,
                &Dims { cols, rows },
                listener.clone(),
            )));

            let ws = WindowSize {
                num_lines: rows as u16,
                num_cols: cols as u16,
                cell_width: cw.round() as u16,
                cell_height: ch.round() as u16,
            };
            let opts = PtyOptions {
                shell: Some(Shell::new(self.config.shell_command(), vec![])),
                ..Default::default()
            };
            let pty = tty::new(&opts, ws, 0).map_err(|e| format!("创建 PTY 失败: {e}"))?;
            let sender = crate::ptyloop::spawn(term.clone(), listener.clone(), pty, self.blocks_tx.clone())
                .map_err(|e| format!("PTY 事件循环创建失败: {e}"))?;
            *listener.sender.lock().unwrap() = Some(sender.clone());
            self.sender = Some(sender);
            self.term = Some(term);

            self.set_ime_area(ctx, (0, 0));
            // 初始 margin 对齐(WindowState 建面时用 -2000 占位防闪屏)。
            ctx.set_top_margin(-self.layer_h);
            ctx.request_redraw();

            // PTY 已 fork:现在才允许起多线程(IPC accept / portal)。
            if let Some(listener) = self.ipc_listener.take() {
                crate::ipc::accept_loop(listener, self.ipc_tx.clone(), self.wake.clone());
            }
            crate::hotkey::spawn(self.hotkey_tx.clone(), self.wake.clone());
            self.threads_started = true;

            Ok(())
        })();
        if let Err(e) = result {
            log::error!("初始化失败: {e}");
            ctx.exit();
        }
    }

    /// 当前光标位置(显示坐标)。
    fn cursor_pos(&self) -> (usize, usize) {
        let Some(term) = &self.term else { return (0, 0) };
        let term = term.lock();
        let content = term.renderable_content();
        let p = content.cursor.point;
        (
            p.column.0,
            (p.line.0 + content.display_offset as i32).max(0) as usize,
        )
    }

    fn set_ime_area(&self, ctx: &mut Ctx<'_>, (col, row): (usize, usize)) {
        if let Some(r) = &self.renderer {
            let (cw, ch) = r.cell_size();
            let pad = self.config.padding;
            ctx.set_ime_cursor_area(col as f32 * cw + pad, row as f32 * ch + pad, cw, ch);
        }
    }

    fn cell_at(&self, x: f64, y: f64) -> Option<(usize, usize)> {
        let (cw, ch) = self.renderer.as_ref()?.cell_size();
        let (cols, rows) = self.dims?;
        let pad = self.config.padding as f64;
        let c = ((x - pad) / cw as f64).floor();
        let r = ((y - pad) / ch as f64).floor();
        if c < 0.0 || r < 0.0 || c as usize >= cols || r as usize >= rows {
            None
        } else {
            Some((c as usize, r as usize))
        }
    }

    /// 从 alacritty_terminal 快照构建渲染网格。
    fn build_grid(&self) -> tay_render::Grid {
        let (cols, rows) = self.dims.unwrap_or((80, 24));
        let term = self.term.as_ref().expect("term 未初始化").lock();
        let content = term.renderable_content();
        let blank = tay_render::Cell {
            c: ' ',
            fg: self.config.colors.foreground,
            bg: self.config.colors.background,
            bold: false,
            italic: false,
            underline: false,
            wide_spacer: false,
        };
        let mut cells = vec![blank; cols * rows];
        for idx in content.display_iter {
            let p = idx.point;
            if p.column.0 >= cols {
                continue;
            }
            let line = p.line.0;
            if line < 0 || line as usize >= rows {
                continue;
            }
            let i = line as usize * cols + p.column.0;
            cells[i] = tay_render::Cell {
                c: idx.cell.c,
                fg: resolve(idx.cell.fg, content.colors, &self.config),
                bg: resolve(idx.cell.bg, content.colors, &self.config),
                bold: idx.cell.flags.contains(Flags::BOLD),
                italic: idx.cell.flags.contains(Flags::ITALIC),
                underline: idx.cell.flags.contains(Flags::UNDERLINE),
                wide_spacer: idx.cell.flags.contains(Flags::WIDE_CHAR_SPACER),
            };
        }
        let cursor_shape = match content.cursor.shape {
            ACursorShape::Block | ACursorShape::HollowBlock => tay_render::CursorShape::Block,
            ACursorShape::Underline => tay_render::CursorShape::Underline,
            ACursorShape::Beam => tay_render::CursorShape::Beam,
            ACursorShape::Hidden => tay_render::CursorShape::Hidden,
        };
        let off = content.display_offset as i32;
        let to_display = |p: Point| {
            (
                p.column.0.min(cols - 1),
                (p.line.0 + off).clamp(0, rows as i32 - 1) as usize,
            )
        };
        let selection = content.selection.map(|s| tay_render::SelectionRange {
            start: to_display(s.start),
            end: to_display(s.end),
        });
        // OSC 133 徽章:非零退出码显示在对应提示符行尾。
        let badges = self
            .blocks
            .iter()
            .filter_map(|(line, code)| {
                let code = (*code)?;
                if code == 0 {
                    return None;
                }
                let row = (line + off) as usize;
                if row >= rows {
                    return None;
                }
                Some(tay_render::Badge {
                    row,
                    text: format!(" ✗ {code} "),
                    color: Rgb::new(0xff, 0x5f, 0x5f),
                })
            })
            .collect();

        // QuickSelect 覆盖层。
        let overlays = self
            .quick
            .as_ref()
            .map(|q| {
                q.matches
                    .iter()
                    .map(|m| tay_render::Overlay { row: m.row, col: m.col, text: m.label.clone() })
                    .collect()
            })
            .unwrap_or_default();

        tay_render::Grid {
            cols,
            rows,
            cells,
            cursor: to_display(content.cursor.point),
            cursor_shape,
            selection,
            preedit: self.preedit.clone(),
            badges,
            overlays,
        }
    }

    /// 跳转到上一条/下一条提示符(DESIGN §8.4「跳转到上一条提示符」)。
    fn jump_prompt(&mut self, dir: i32) {
        let Some(term) = &self.term else { return };
        let top = -(term.lock().grid().display_offset() as i32);
        let target = if dir < 0 {
            self.blocks.iter().rev().map(|b| b.0).find(|l| *l < top)
        } else {
            self.blocks.iter().map(|b| b.0).find(|l| *l > top)
        };
        if let Some(line) = target {
            term.lock().scroll_to_point(Point::new(Line(line), Column(0)));
        }
    }

    /// 复制上一条命令的输出(§8.4「按块复制」)。
    fn copy_command_output(&mut self) {
        let Some(term) = self.term.as_ref() else { return };
        let n = self.blocks.len();
        if n < 2 {
            return;
        }
        let (from_line, _) = self.blocks[n - 2];
        let (to_line, _) = self.blocks[n - 1];
        if to_line - from_line <= 1 {
            return;
        }
        let term = term.lock();
        let cols = term.grid().columns().saturating_sub(1);
        let text = term.bounds_to_string(
            Point::new(Line(from_line + 1), Column(0)),
            Point::new(Line(to_line - 1), Column(cols)),
        );
        if !text.is_empty() {
            let len = text.len();
            if let Ok(mut cb) = arboard::Clipboard::new() {
                let _ = cb.set_text(text);
                log::info!("已复制上一条命令的输出({len} 字节)");
            }
        }
    }

    /// 选区复制:优先自己的鼠标选区,其次 term 的(vi 模式)选区。
    fn copy_selection(&mut self) {
        let Some(term) = self.term.as_ref() else { return };
        let text = if let Some((start, end)) = self.sel {
            let (s, e) = (start.min(end), start.max(end));
            let term = term.lock();
            let off = term.grid().display_offset() as i32;
            let start = Point::new(Line(s.1 as i32 - off), Column(s.0));
            let end = Point::new(Line(e.1 as i32 - off), Column(e.0));
            Some(term.bounds_to_string(start, end))
        } else {
            term.lock().selection_to_string()
        };
        if let Some(text) = text {
            if let Ok(mut cb) = arboard::Clipboard::new() {
                let _ = cb.set_text(text);
            }
        }
    }

    fn render(&mut self, ctx: &mut Ctx<'_>) {
        let t0 = Instant::now();
        let Some(_) = self.renderer.as_ref() else { return };
        self.frames += 1;

        // 热呼出延迟:呼出指令 → 首帧。
        if let Some(at) = self.show_at.take() {
            self.shown_once = true;
            log::info!("热呼出 {}ms", at.elapsed().as_millis());
        }

        // TAY_QUICKSELECT:第 12 帧自动触发(验证用)。
        if self.quickselect_at_start && self.frames == 12 {
            self.quickselect_at_start = false;
            let grid = self.build_grid();
            let matches = crate::quickselect::build(&grid);
            log::info!("QuickSelect: {} 个匹配项", matches.len());
            self.quick = Some(QuickState { matches, typed: String::new() });
        }

        // TAY_EXEC:第 10 帧执行命令(等 shell 就绪)。
        if !self.exec_sent && self.frames >= 10 {
            self.exec_sent = true;
            if let (Some(sender), Some(cmd)) = (&self.sender, &self.exec) {
                log::info!("执行: {cmd}");
                let _ = sender.send(Msg::Input(format!("{cmd}\n").into_bytes()));
            }
        }

        // 基准:启动后第 20 帧灌入 3000 行输出。
        if let Some(bench) = self.bench {
            if !self.bench_started && self.frames >= 20 {
                self.bench_started = true;
                log::info!("基准:灌入 3000 行输出");
                if let Some(sender) = &self.sender {
                    let cmd = "i=0; while [ $i -lt 3000 ]; do echo $i; i=$((i+1)); done\n";
                    let _ = sender.send(Msg::Input(cmd.as_bytes().to_vec()));
                }
                let _ = bench;
            }
        }
        let grid = self.build_grid();
        let alpha = self.config.opacity.clamp(0.0, 1.0);
        {
            let renderer = self.renderer.as_mut().expect("renderer 未初始化");
            renderer.update(&grid);
            match renderer.acquire_texture() {
                wgpu::CurrentSurfaceTexture::Success(texture)
                | wgpu::CurrentSurfaceTexture::Suboptimal(texture) => {
                    ctx.pre_present_notify();
                    if let Err(e) = renderer.render(texture, alpha) {
                        log::error!("渲染失败: {e}");
                    }
                },
                wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                    let (w, h) = renderer.surface_size();
                    renderer.resize(w, h);
                },
                wgpu::CurrentSurfaceTexture::Timeout
                | wgpu::CurrentSurfaceTexture::Occluded
                | wgpu::CurrentSurfaceTexture::Validation => {},
            }
        }

        // TAY_SHOT / TAY_BENCH:连续重绘,驱动帧计数。
        let continuous = self.renderer.as_ref().is_some_and(|r| r.shot_pending())
            || self.bench.is_some_and(|n| (self.bench_times.len() as u32) < n);
        if continuous {
            ctx.set_poll(true);
            ctx.request_redraw();
        }

        // TAY_SHOT:截图完成后退出。
        if self.renderer.as_ref().is_some_and(|r| r.shot_taken()) {
            log::info!("截图完成,退出");
            ctx.exit();
            return;
        }

        // TAY_BENCH:统计帧时间。
        if self.bench_started {
            self.bench_times.push(t0.elapsed().as_secs_f32() * 1000.0);
            if let Some(n) = self.bench {
                if self.bench_times.len() as u32 >= n {
                    self.bench_times.sort_by(|a, b| a.partial_cmp(b).unwrap());
                    let avg = self.bench_times.iter().sum::<f32>() / self.bench_times.len() as f32;
                    let p50 = self.bench_times[self.bench_times.len() / 2];
                    let p99 = self.bench_times[(self.bench_times.len() as f32 * 0.99) as usize];
                    let max = *self.bench_times.last().unwrap();
                    log::info!(
                        "基准(滚动输出下 {n} 帧):avg {avg:.2}ms p50 {p50:.2}ms p99 {p99:.2}ms max {max:.2}ms ≈ {:.0} fps",
                        1000.0 / avg.max(0.001)
                    );
                    ctx.exit();
                }
            }
        }
    }
}

impl Handler for TayApp {
    fn on_event(&mut self, ctx: &mut Ctx<'_>, event: WEvent) {
        match event {
            WEvent::CanCreateSurfaces => {
                // 第一 configure 后:按输出高度重设 layer 尺寸,
                // 等待 Resized 确认后一次性初始化渲染器(只 configure 一次,
                // 规避 Mesa WSI 二次 swapchain 重建的段错误)。
                if self.sender.is_none() {
                    let (_, out_h) = ctx.output_logical_size().unwrap_or((1920, 1080));
                    let h = (out_h * self.config.height_percent / 100).max(200);
                    self.layer_h = h as i32;
                    log::info!("layer 高度 {h}px(输出 {out_h}px 高)");
                    ctx.set_size(0, h);
                    self.sizing = true;
                }
            },

            WEvent::Resized { width, height } => {
                if self.sizing && height as i32 >= self.layer_h - 1 && height as i32 <= self.layer_h + 1 {
                    self.sizing = false;
                    self.init(ctx);
                    // M1 常驻模型:默认收起;TAY_START_VISIBLE / 配置可让启动即呼出。
                    if !self.config.start_hidden {
                        self.show(ctx);
                    }
                    return;
                }
                if let Some(r) = &mut self.renderer {
                    if r.surface_size() == (width, height) {
                        return;
                    }
                    r.resize(width, height);
                    let (cw, ch) = r.cell_size();
                    let pad = self.config.padding;
                    let cols = ((width as f32 - 2.0 * pad) / cw).floor().max(2.0) as usize;
                    let rows = ((height as f32 - 2.0 * pad) / ch).floor().max(1.0) as usize;
                    if Some((cols, rows)) != self.dims {
                        log::info!("网格调整 {cols}×{rows}");
                        self.dims = Some((cols, rows));
                        if let Some(term) = &self.term {
                            term.lock().resize(Dims { cols, rows });
                        }
                        if let Some(sender) = &self.sender {
                            let _ = sender.send(Msg::Resize(WindowSize {
                                num_lines: rows as u16,
                                num_cols: cols as u16,
                                cell_width: cw.round() as u16,
                                cell_height: ch.round() as u16,
                            }));
                        }
                    }
                }
                ctx.request_redraw();
            },

            WEvent::Focused(focused) => {
                // 失焦自动隐藏(DESIGN §6.3:Wayland 上即 keyboard leave)。
                if !focused && self.config.hide_on_focus_out && self.vis != Vis::Hidden {
                    self.hide(ctx);
                }
            },

            WEvent::Keyboard { key, pressed, mods } => {
                if !pressed {
                    return;
                }
                // QuickSelect 激活:消费按键(标签 / Esc 取消)。
                if self.quick.is_some() {
                    match key.key {
                        KeyKind::Named(tay_window::NamedKey::Escape) => {
                            self.quick = None;
                            ctx.request_redraw();
                            return;
                        },
                        KeyKind::Char(c) if c.is_ascii_lowercase() => {
                            let fire = {
                                let q = self.quick.as_mut().unwrap();
                                q.typed.push(c);
                                // 前缀无匹配 → 取消。
                                let any_prefix = q.matches.iter().any(|m| m.label.starts_with(&q.typed));
                                if !any_prefix {
                                    self.quick = None;
                                    return;
                                }
                                q.resolve().is_some()
                            };
                            if fire {
                                let q = self.quick.take().unwrap();
                                if let Some(m) = q.resolve() {
                                    crate::quickselect::activate(m);
                                }
                            }
                            ctx.request_redraw();
                            return;
                        },
                        _ => {
                            // 其他按键取消。
                            self.quick = None;
                            ctx.request_redraw();
                            return;
                        },
                    }
                }
                // 触发 QuickSelect:Ctrl+Shift+F。
                if mods.ctrl && mods.shift && matches!(key.key, KeyKind::Char('f') | KeyKind::Char('F')) {
                    let grid = self.build_grid();
                    let matches = crate::quickselect::build(&grid);
                    if matches.is_empty() {
                        log::info!("QuickSelect: 无匹配项");
                    } else {
                        log::info!("QuickSelect: {} 个匹配项,按键选择", matches.len());
                        self.quick = Some(QuickState { matches, typed: String::new() });
                    }
                    ctx.request_redraw();
                    return;
                }
                // 剪贴板/命令块快捷键:Ctrl+Shift+字母与方向键。
                if mods.ctrl && mods.shift {
                    match key.key {
                        KeyKind::Char('c') | KeyKind::Char('C') => {
                            self.copy_selection();
                            return;
                        },
                        KeyKind::Char('v') | KeyKind::Char('V') => {
                            if let Ok(mut cb) = arboard::Clipboard::new() {
                                if let Ok(text) = cb.get_text() {
                                    if let Some(sender) = &self.sender {
                                        let _ = sender.send(Msg::Input(text.into_bytes()));
                                    }
                                }
                            }
                            return;
                        },
                        // 命令块:跳转提示符 / 复制上一条命令输出。
                        KeyKind::Named(tay_window::NamedKey::ArrowUp) => {
                            self.jump_prompt(-1);
                            ctx.request_redraw();
                            return;
                        },
                        KeyKind::Named(tay_window::NamedKey::ArrowDown) => {
                            self.jump_prompt(1);
                            ctx.request_redraw();
                            return;
                        },
                        KeyKind::Char('o') | KeyKind::Char('O') => {
                            self.copy_command_output();
                            return;
                        },
                        _ => {},
                    }
                }
                let app_cursor = self
                    .term
                    .as_ref()
                    .is_some_and(|t| t.lock().mode().contains(TermMode::APP_CURSOR));
                if let Some(bytes) = crate::keys::translate(&key, mods, app_cursor) {
                    if let Some(sender) = &self.sender {
                        let _ = sender.send(Msg::Input(bytes));
                    }
                }
                let pos = self.cursor_pos();
                self.set_ime_area(ctx, pos);
                ctx.request_redraw();
            },

            WEvent::Ime(ime) => match ime {
                ImeEvent::Commit(text) => {
                    if let Some(sender) = &self.sender {
                        let _ = sender.send(Msg::Input(text.into_bytes()));
                    }
                    self.preedit = None;
                    ctx.request_redraw();
                },
                ImeEvent::Preedit { text, cursor } => {
                    if text.is_empty() {
                        self.preedit = None;
                    } else {
                        let (col, row) = self.cursor_pos();
                        self.preedit = Some(tay_render::Preedit { text, col, row, cursor_bytes: cursor });
                        self.set_ime_area(ctx, (col, row));
                    }
                    ctx.request_redraw();
                },
                _ => {},
            },

            WEvent::PointerButton { button, pressed, x, y } => {
                if button == MouseButtonIdx::Left {
                    if pressed {
                        self.dragging = true;
                        let cell = self.cell_at(x, y).unwrap_or((0, 0));
                        self.sel = Some((cell, cell));
                    } else {
                        self.dragging = false;
                        if let Some((s, e)) = self.sel {
                            if s == e {
                                self.sel = None;
                            }
                        }
                    }
                    ctx.request_redraw();
                }
            },

            WEvent::PointerMoved { x, y } => {
                if self.dragging {
                    if let Some(cell) = self.cell_at(x, y) {
                        if let Some((start, _)) = self.sel {
                            self.sel = Some((start, cell));
                        }
                    }
                    ctx.request_redraw();
                }
            },

            WEvent::Scroll { lines_y, pixel_y, .. } => {
                let mut delta = lines_y as i32 * 3;
                if lines_y == 0.0 && pixel_y != 0.0 {
                    self.scroll_px += pixel_y;
                    let ch = self.renderer.as_ref().map(|r| r.cell_size().1).unwrap_or(20.0);
                    delta = (self.scroll_px / ch) as i32;
                    self.scroll_px -= delta as f32 * ch;
                }
                if delta != 0 {
                    if let Some(term) = &self.term {
                        term.lock().scroll_display(Scroll::Delta(delta));
                    }
                    ctx.request_redraw();
                }
            },

            WEvent::RedrawRequested => {
                self.render(ctx);
            },

            WEvent::ProxyWake => {
                // 终端事件。
                while let Ok(ev) = self.rx.try_recv() {
                    match ev {
                        Event::Wakeup => ctx.request_redraw(),
                        Event::Title(t) => log::debug!("标题: {t}"),
                        Event::ClipboardStore(_, text) => {
                            if let Ok(mut cb) = arboard::Clipboard::new() {
                                let _ = cb.set_text(text);
                            }
                        },
                        Event::Exit | Event::ChildExit(_) => {
                            log::info!("shell 已退出");
                            ctx.exit();
                        },
                        _ => {},
                    }
                }
                // IPC 命令(`tay toggle` 等)。
                while let Ok(cmd) = self.ipc_rx.try_recv() {
                    match cmd {
                        crate::ipc::IpcCommand::Toggle => self.toggle(ctx),
                        crate::ipc::IpcCommand::Show => self.show(ctx),
                        crate::ipc::IpcCommand::Hide => self.hide(ctx),
                        crate::ipc::IpcCommand::Quit => {
                            log::info!("收到 quit");
                            if let Some(sender) = &self.sender {
                                let _ = sender.send(Msg::Shutdown);
                            }
                            ctx.exit();
                        },
                    }
                }
                // portal 热键。
                while self.hotkey_rx.try_recv().is_ok() {
                    self.toggle(ctx);
                }
                // OSC 133 命令块标记。
                while let Ok(ev) = self.blocks_rx.try_recv() {
                    match ev {
                        BlockEvent::PromptStart(line) => {
                            self.blocks.push((line, None));
                            if self.blocks.len() > 200 {
                                self.blocks.drain(..self.blocks.len() - 200);
                            }
                        },
                        BlockEvent::Exit { code } => {
                            if let Some(last) = self.blocks.last_mut() {
                                last.1 = Some(code);
                            }
                        },
                    }
                }
            },

            WEvent::CloseRequested => ctx.exit(),

            WEvent::AboutToWait => {
                // 呼出/收起动画步进。
                self.step_animation(ctx);
                // 截图/基准模式:事件循环自身驱动连续帧。
                let continuous = self.renderer.as_ref().is_some_and(|r| r.shot_pending())
                    || self.bench.is_some_and(|n| (self.bench_times.len() as u32) < n);
                if continuous {
                    ctx.set_poll(true);
                    ctx.request_redraw();
                } else {
                    ctx.set_poll(false);
                }
            },

            _ => {},
        }
    }
}
