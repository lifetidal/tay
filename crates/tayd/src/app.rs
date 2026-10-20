//! TayApp:终端内核 + PTY + 窗口 + 渲染 + 呼出状态机(M1)。
//!
//! 数据流:
//! - 键盘/IME → `SessionCommand::Input` → PTY → shell → 输出 → 会话解析 → 快照 + `Wakeup` → 重绘
//! - 呼出/收起 = layer margin 动画(DESIGN §8.1):GPU surface 常驻,零重建
//! - 热键(portal)/ IPC(`tay toggle`)/ 失焦(keyboard leave)三者汇入同一状态机

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::grid::Scroll;
use alacritty_terminal::index::Point;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors as TermColors;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::tty::{self, Options as PtyOptions, Shell};
use alacritty_terminal::vi_mode::ViMotion;
use alacritty_terminal::vte::ansi::{Color, CursorShape as ACursorShape, CursorStyle, NamedColor};

use crate::ptyloop::TermSender;
use crate::session::{
    AnchorEpoch, DisplayPoint, GridPoint, HistoryAnchor, SessionCommand, SessionReply,
    SessionSnapshot, SessionState, SessionUpdate,
};
use tay_config::{Config, Rgb};
use tay_render::wgpu;
use tay_window::{Ctx, Event as WEvent, Handler, ImeEvent, KeyKind, MouseButtonIdx};

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
        colors[i]
            .map(|r| Rgb::new(r.r, r.g, r.b))
            .unwrap_or(fallback)
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
                from_palette(n as usize, dim(cfg.colors.ansi[base]))
            }
            _ => from_palette(n as usize, cfg.colors.ansi[n as usize % 16]),
        },
        Color::Spec(r) => Rgb::new(r.r, r.g, r.b),
        Color::Indexed(i) => from_palette(i as usize, cfg.colors.indexed(i)),
    }
}

/// 终端事件转发:PTY 线程 → UI 线程。
#[derive(Clone)]
struct Listener {
    tx: mpsc::Sender<Event>,
    wake: Arc<UnixStream>,
    sender: Arc<Mutex<Option<TermSender>>>,
}

impl Listener {
    fn new(tx: mpsc::Sender<Event>, wake: Arc<UnixStream>) -> Self {
        Self {
            tx,
            wake,
            sender: Arc::new(Mutex::new(None)),
        }
    }
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        match event {
            // term 要写给 PTY 的字节(对查询序列的应答等):必须
            // 送达 —— 丢弃协议应答会卡住应用;经 send_response
            // 绕过输入预算上限但计入在途(统一记账,会话侧按实际
            // 写入释放)。
            Event::PtyWrite(text) => {
                if let Some(s) = self.sender.lock().unwrap().as_ref() {
                    let _ = s.send_response(text.into_bytes());
                }
            }
            event => {
                let _ = self.tx.send(event);
                let _ = (&*self.wake).write(&[0]);
            }
        }
    }
}

struct TabSession {
    sender: TermSender,
    slot: Arc<Mutex<Option<SessionUpdate>>>,
    snapshot: Option<Arc<SessionSnapshot>>,
    tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
    reply_tx: mpsc::Sender<SessionReply>,
    reply_rx: mpsc::Receiver<SessionReply>,
    title: String,
    /// 折叠状态按**稳定 BlockId** 键控,属该标签(切换标签保留,
    /// 与 alt-screen 切换同理 —— 折叠不得因离开标签而丢失)。
    folded: std::collections::BTreeSet<crate::blocks::BlockId>,
}

pub struct TayApp {
    tabs: Vec<Option<TabSession>>,
    active_tab: usize,
    tab_title: String,
    settings_open: bool,
    hotkey_status: String,
    chrome_hover: Option<crate::chrome::Action>,
    maximized: bool,
    editing: Option<(crate::chrome::Field, String)>,

    config: Config,
    shot: Option<(String, u32)>,
    bench: Option<u32>,

    instance: Option<wgpu::Instance>,
    renderer: Option<tay_render::Renderer>,
    sender: Option<TermSender>,
    tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
    dims: Option<(usize, usize)>,
    preedit: Option<tay_render::Preedit>,
    /// 鼠标选区(稳定锚点):(起点, 当前点) —— 行号为历史绝对行、
    /// 列为网格列。锚点在**选择时刻**定格,后续滚屏/输出不改变它
    /// 指向的文本(P1:异步复制不受请求前的滚屏影响)。
    sel: Option<((HistoryAnchor, u32), (HistoryAnchor, u32))>,
    /// 选区来源快照的失效世代(选择时刻定格):复制请求携带它,
    /// 会话执行前校验 —— 清屏/resize/屏幕切换后旧选择明确返回
    /// 过期,绝不把新文本当旧选择复制(第八轮定点修复)。
    sel_epoch: AnchorEpoch,
    dragging: bool,
    scroll_px: f32,

    // —— M1:呼出状态(P3c:状态机拆到 visibility.rs)——
    vis: crate::visibility::VisibilityController,
    // —— M3:QuickSelect / vi 复制模式 ——
    quick: Option<QuickState>,
    vi_search_query: Option<String>,
    /// 显示映射(架构 §2.3:TerminalView;折叠感知正向/反向转换)。
    view: crate::view::ViewMap,
    /// 折叠状态(UI 视图状态,按稳定 BlockId;行号变化不影响)。
    folded: std::collections::BTreeSet<crate::blocks::BlockId>,
    /// 最近见过的网格失效代数(变化 → 丢弃旧视图状态)。
    seen_epoch: AnchorEpoch,
    /// GPU surface 生命周期阶段(架构 §5.1)。
    gpu_phase: GpuPhase,
    layer_h: i32,
    shown_once: bool,
    show_at: Option<Instant>,
    ipc_listener: Option<std::os::unix::net::UnixListener>,
    ipc_tx: mpsc::Sender<crate::ipc::IpcCommand>,
    ipc_rx: mpsc::Receiver<crate::ipc::IpcCommand>,
    hotkey_tx: mpsc::Sender<crate::hotkey::HotkeyTriggered>,
    hotkey_rx: mpsc::Receiver<crate::hotkey::HotkeyTriggered>,
    wake: Arc<UnixStream>,
    /// init 完成后启动的后台线程(PTY fork 之后才能起)。
    threads_started: bool,

    frames: u32,
    bench_started: bool,
    bench_times: Vec<f32>,
    exec: Option<String>,
    exec_sent: bool,
    quickselect_at_start: bool,
    vi_at_start: bool,
    fold_at_start: bool,
    /// 验收钩子:第 11 帧注入的 IME Commit 文本。
    ime_at_start: Option<String>,
    /// 验收钩子:第 15 帧强制 surface 重建。
    rebind_at_start: bool,
    /// 验收钩子:第 16 帧注入滚轮滚动、第 17 帧合成拖选复制、
    /// 第 18/19 帧注入两次 300 KiB 大段粘贴(统一输入预算端到端
    /// 验证)。
    scroll_at_start: bool,
    sel_at_start: bool,
    paste_at_start: bool,
    /// 验收钩子:第 15..80 帧渲染前人为延迟 ~30ms(慢 UI 模拟 ——
    /// 验证快照单槽覆盖、输入预算与语义状态在 UI 卡顿下不积压、
    /// 不触发 stalled 看门狗)。
    slowui_at_start: bool,
    config_mtime: Option<std::time::SystemTime>,
    config_reload: Option<mpsc::Receiver<Result<Config, String>>>,
    kitty_down: std::collections::BTreeMap<u32, tay_window::KeyPress>,
    /// 最近一次热重载失败的配置 mtime(同一版本只告警一次,修复后自动恢复)。
    config_error_at: Option<std::time::SystemTime>,
    /// 持久剪贴板(Wayland 上数据归属存活连接:每用新建即弃可能丢数据,
    /// 且每次 new 都有往返开销 —— 常驻一个实例,全部读写复用)。
    clipboard: Option<arboard::Clipboard>,
    /// 状态 toast(QuickSelect 反馈等):(消息, 置入时间)。
    status_msg: Option<(String, Instant)>,
    /// 会话:快照单槽(会话线程发布,UI 渲染只读它 —— 架构 §3)。
    snapshot_slot: Option<Arc<Mutex<Option<SessionUpdate>>>>,
    /// 最近一次快照(渲染/输入路由/光标定位的唯一来源)。
    snapshot: Option<Arc<SessionSnapshot>>,
    /// 会话应答发送端(复制/搜索回执;接收端在 ProxyWake 排空)。
    reply_tx: Option<mpsc::Sender<SessionReply>>,
    reply_rx: Option<mpsc::Receiver<SessionReply>>,
}

/// GPU surface 生命周期(对应窗口层 surface 状态,架构 §5.1):
/// 等待 configure 期间不能创建/使用渲染器;布尔组合被显式阶段取代。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GpuPhase {
    /// 渲染器就绪(或尚待首次创建)。
    Ready,
    /// 已请求尺寸/重建,等 configure(Resized)握手;rebuild =
    /// 需要重建渲染器(输出回退)vs 只重设高度。
    AwaitingConfigure { rebuild: bool },
}

impl GpuPhase {
    /// 高度请求的状态转换(纯函数,可测):仅在 Ready 时推进到
    /// AwaitingConfigure{rebuild:false};已 AwaitingConfigure 保持
    /// 原标志 —— **重建需求不可被高度请求覆盖**(第十三轮修复的
    /// 卡死回归,这里固化为受测转换)。
    fn on_height_request(self) -> GpuPhase {
        match self {
            GpuPhase::Ready => GpuPhase::AwaitingConfigure { rebuild: false },
            other => other,
        }
    }

    /// Resized 握手后的决策:返回(是否重建渲染器);其余分支由
    /// 调用方按 sender 是否初始化区分。
    fn on_resized_handshake(self) -> bool {
        match self {
            GpuPhase::AwaitingConfigure { rebuild } => rebuild,
            GpuPhase::Ready => false,
        }
    }
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
    // 初始化参数全部来自 main() 的启动装配(环境变量/管道端点),
    // 一次性传入比引入 builder 更直白 —— 参数个数是装配点的固有形状。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Config,
        shot: Option<(String, u32)>,
        bench: Option<u32>,
        exec: Option<String>,
        quickselect_at_start: bool,
        vi_at_start: bool,
        fold_at_start: bool,
        ime_at_start: Option<String>,
        rebind_at_start: bool,
        scroll_at_start: bool,
        sel_at_start: bool,
        paste_at_start: bool,
        slowui_at_start: bool,
        ipc_listener: Option<std::os::unix::net::UnixListener>,
        ipc_tx: mpsc::Sender<crate::ipc::IpcCommand>,
        ipc_rx: mpsc::Receiver<crate::ipc::IpcCommand>,
        hotkey_tx: mpsc::Sender<crate::hotkey::HotkeyTriggered>,
        hotkey_rx: mpsc::Receiver<crate::hotkey::HotkeyTriggered>,
        wake: Arc<UnixStream>,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let hotkey_status = format!("{}: requesting system binding", config.global_shortcut);
        Self {
            config,
            shot,
            bench,
            instance: None,
            renderer: None,
            sender: None,
            tx,
            rx,
            tabs: vec![None],
            active_tab: 0,
            tab_title: "shell".into(),
            settings_open: false,
            hotkey_status,
            chrome_hover: None,
            maximized: false,
            editing: None,
            dims: None,
            preedit: None,
            sel: None,
            sel_epoch: AnchorEpoch::default(),
            dragging: false,
            scroll_px: 0.0,
            vis: crate::visibility::VisibilityController::new(480),
            quick: None,
            vi_search_query: None,
            view: crate::view::ViewMap::default(),
            gpu_phase: GpuPhase::Ready,
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
            vi_at_start,
            fold_at_start,
            ime_at_start,
            rebind_at_start,
            scroll_at_start,
            sel_at_start,
            paste_at_start,
            slowui_at_start,
            config_mtime: None,
            config_reload: None,
            kitty_down: Default::default(),
            config_error_at: None,
            clipboard: arboard::Clipboard::new().ok(),
            status_msg: None,
            snapshot_slot: None,
            snapshot: None,
            folded: Default::default(),
            seen_epoch: AnchorEpoch::default(),
            reply_tx: None,
            reply_rx: None,
        }
    }

    /// 呼出(margin 动画到 0;状态机见 visibility.rs,
    /// 此处提交 margin + 重绘;无会话时先启动新会话)。
    fn show(&mut self, ctx: &mut Ctx<'_>) {
        if self.sender.is_none()
            && let Some((cols, rows)) = self.dims
        {
            match self.start_session(cols, rows) {
                Ok(tab) => self.install_session(tab),
                Err(e) => self.status(format!("无法启动 Shell: {e}"), ctx),
            }
        }
        // 呼出是用户显式意图:清除遮挡 watchdog 的 stalled(隐藏
        // 期间合成器不回帧回调,旧实现在 stalled 后丢弃全部重绘,
        // 窗口永远呼不出 —— 见 tay-window 的 wake_present 注释)。
        ctx.wake_present();
        match self.vis.show(self.config.animation_ms) {
            crate::visibility::VisOutcome::None => return,
            crate::visibility::VisOutcome::Margin(m) => ctx.set_top_margin(m),
            crate::visibility::VisOutcome::Animating { .. } => {}
        }
        ctx.request_keyboard(true, true);
        self.show_at = Some(Instant::now());
        // 呼出即渲染首帧(内容可能已过时;同时完成热呼出延迟测量)。
        ctx.request_redraw();
    }

    /// 收起。
    fn hide(&mut self, ctx: &mut Ctx<'_>) {
        ctx.request_keyboard(false, false);
        match self.vis.hide(self.config.animation_ms) {
            crate::visibility::VisOutcome::None => {}
            crate::visibility::VisOutcome::Margin(m) => ctx.set_top_margin(m),
            crate::visibility::VisOutcome::Animating { .. } => {}
        }
    }

    /// toggle:动画中则反向(从当前实际位置,而非动画起点)。
    fn toggle(&mut self, ctx: &mut Ctx<'_>) {
        if self.sender.is_none() {
            self.show(ctx);
            return;
        }
        // 呼出/切换是用户显式意图:清除遮挡 watchdog 的 stalled
        // (隐藏启动 2s 后 toggle 的修复,与 show 同因)。
        ctx.wake_present();
        match self.vis.toggle(self.config.animation_ms) {
            crate::visibility::VisOutcome::None => {}
            crate::visibility::VisOutcome::Margin(m) => {
                ctx.set_top_margin(m);
                if self.vis.is_shown() {
                    ctx.request_keyboard(true, true);
                    // 无动画直达呼出:同样要延迟测量与首帧。
                    self.show_at = Some(Instant::now());
                    ctx.request_redraw();
                }
            }
            crate::visibility::VisOutcome::Animating { showing } => {
                if showing {
                    ctx.request_keyboard(true, true);
                    // 呼出侧转换:启动热呼出测量并渲染首帧。
                    self.show_at = Some(Instant::now());
                    ctx.request_redraw();
                }
            }
        }
        if self.vis.is_hidden() {
            ctx.request_keyboard(false, false);
        }
    }

    /// 动画步进(AboutToWait 驱动):按实际耗时计算 margin,只在实际
    /// 变化时提交;并向事件循环报告下一动画步的 deadline(空闲阻塞
    /// 时据此醒来,P2a)。
    /// 动画步进(AboutToWait 驱动):
    /// - margin 有变化才提交(取整可能连续多步不变);
    /// - **只要仍在动画,就必须保留下一次唤醒 deadline** ——
    ///   "本次无需提交 margin" 不等于"动画结束",否则取整平台期
    ///   或高频事件会让动画冻结、永远无法进入最终隐藏状态。
    fn step_animation(&mut self, ctx: &mut Ctx<'_>) {
        let was_animating = self.vis.animating();
        if let Some(margin) = self.vis.step(self.config.animation_ms) {
            ctx.set_top_margin(margin);
        }
        if was_animating && self.vis.is_hidden() {
            ctx.request_keyboard(false, false);
        }
        if self.vis.animating() {
            // 下一动画步(~8ms 粒度,≈125 步/秒,150ms 动画 ≈ 19 步)。
            ctx.set_anim_deadline(Some(Instant::now() + Duration::from_millis(8)));
        } else {
            ctx.set_anim_deadline(None);
        }
    }

    /// 从当前窗口层的 (display, surface) 句柄创建 wgpu surface 与
    /// 渲染器。首次初始化与输出回退重建共用;返回网格 (cols, rows)。
    /// 注意:重建前调用方必须先 drop 旧 renderer(旧 wl_surface 由
    /// 窗口层 retired 保活,GPU 销毁 swapchain 时仍需 marshal 它)。
    fn create_renderer(&mut self, ctx: &mut Ctx<'_>) -> Result<(usize, usize), String> {
        let (disp_ptr, surf_ptr) = ctx.raw_wayland().ok_or("取 Wayland 句柄失败")?;
        let mut instance_desc = wgpu::InstanceDescriptor::new_without_display_handle();
        instance_desc.backends = wgpu::Backends::PRIMARY | wgpu::Backends::GL;
        let instance = wgpu::Instance::new(instance_desc);
        let raw_display =
            wgpu::rwh::RawDisplayHandle::Wayland(wgpu::rwh::WaylandDisplayHandle::new(
                std::ptr::NonNull::new(disp_ptr as *mut _).expect("display 指针非空"),
            ));
        let raw_window = wgpu::rwh::RawWindowHandle::Wayland(wgpu::rwh::WaylandWindowHandle::new(
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
        let pad = renderer.padding();
        let cols = ((size.0 as f32 - 2.0 * pad) / cw).floor().max(2.0) as usize;
        let rows = (((size.1 as f32 - 2.0 * pad) / ch).floor() as usize)
            .saturating_sub(crate::chrome::ROWS)
            .max(1);
        log::info!("网格 {cols}×{rows} @ {cw:.1}×{ch:.1}px 单元格");
        self.dims = Some((cols, rows));
        self.renderer = Some(renderer);
        self.instance = Some(instance);
        Ok((cols, rows))
    }

    fn start_session(&self, cols: usize, rows: usize) -> Result<TabSession, String> {
        // 终端 + PTY 事件循环线程。
        let (tx, rx) = mpsc::channel();
        let listener = Listener::new(tx.clone(), self.wake.clone());
        let term_config = TermConfig {
            scrolling_history: 10_000,
            vi_mode_cursor_style: Some(CursorStyle {
                shape: ACursorShape::Beam,
                blinking: false,
            }),
            ..Default::default()
        };
        let state = SessionState::new(Term::new(
            term_config,
            &Dims { cols, rows },
            listener.clone(),
        ));
        // 锚点行距上限 = 滚动历史 + 屏幕行数(与网格淘汰对齐)。

        let (cw, ch) = self
            .renderer
            .as_ref()
            .map(|r| r.cell_size())
            .unwrap_or((8.0, 16.0));
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
        // 快照单槽 + 应答通道(架构 §3):Term 所有权随 spawn 移入
        // 会话线程,UI 只留句柄。
        let slot: Arc<Mutex<Option<SessionUpdate>>> = Arc::new(Mutex::new(None));
        let (reply_tx, reply_rx) = mpsc::channel();
        let sender = crate::ptyloop::spawn(state, listener.clone(), pty, slot.clone())
            .map_err(|e| format!("PTY 事件循环创建失败: {e}"))?;
        *listener.sender.lock().unwrap() = Some(sender.clone());
        Ok(TabSession {
            sender,
            slot,
            snapshot: None,
            tx,
            rx,
            reply_tx,
            reply_rx,
            title: "shell".into(),
            folded: Default::default(),
        })
    }

    fn install_session(&mut self, tab: TabSession) {
        self.kitty_down.clear();
        self.sender = Some(tab.sender);
        self.snapshot = tab
            .slot
            .lock()
            .unwrap()
            .take()
            .map(|u| u.snapshot)
            .or(tab.snapshot);
        self.snapshot_slot = Some(tab.slot);
        self.tx = tab.tx;
        self.rx = tab.rx;
        self.reply_tx = Some(tab.reply_tx);
        self.reply_rx = Some(tab.reply_rx);
        self.tab_title = tab.title;
        self.folded = tab.folded;
        self.sel = None;
        self.quick = None;
        self.preedit = None;
        self.dragging = false;
        self.vi_search_query = None;
        self.scroll_px = 0.0;
        self.status_msg = None;
        self.seen_epoch = self
            .snapshot
            .as_ref()
            .map(|s| s.meta.invalidate_epoch)
            .unwrap_or_default();
        self.view = Default::default();
    }

    fn switch_tab(&mut self, index: usize, ctx: &mut Ctx<'_>) {
        if index == self.active_tab || index >= self.tabs.len() {
            return;
        }
        let Some(next) = self.tabs[index].take() else {
            return;
        };
        let (_, empty_rx) = mpsc::channel();
        let old = TabSession {
            sender: self.sender.take().expect("active session"),
            slot: self.snapshot_slot.take().expect("active snapshot slot"),
            snapshot: self.snapshot.take(),
            tx: self.tx.clone(),
            rx: std::mem::replace(&mut self.rx, empty_rx),
            reply_tx: self.reply_tx.take().expect("active reply sender"),
            reply_rx: self.reply_rx.take().expect("active reply receiver"),
            title: self.tab_title.clone(),
            folded: std::mem::take(&mut self.folded),
        };
        self.tabs[self.active_tab] = Some(old);
        self.active_tab = index;
        self.install_session(next);
        self.resize_active();
        ctx.request_redraw();
    }

    fn resize_active(&self) {
        if let (Some((cols, rows)), Some(r), Some(sender)) =
            (self.dims, &self.renderer, &self.sender)
        {
            let (cw, ch) = r.cell_size();
            let _ = sender.send(SessionCommand::Resize(WindowSize {
                num_cols: cols as u16,
                num_lines: rows as u16,
                cell_width: cw.round() as u16,
                cell_height: ch.round() as u16,
            }));
        }
    }

    fn new_tab(&mut self, ctx: &mut Ctx<'_>) {
        if self.sender.is_none() {
            self.show(ctx);
            return;
        }
        if self.tabs.len() >= 24 {
            self.status("最多 24 个标签页", ctx);
            return;
        }
        let (cols, rows) = self.dims.unwrap_or((80, 24));
        match self.start_session(cols, rows) {
            Ok(tab) => {
                self.tabs.push(Some(tab));
                self.switch_tab(self.tabs.len() - 1, ctx);
            }
            Err(e) => self.status(format!("新建标签失败: {e}"), ctx),
        }
    }

    fn close_tab(&mut self, index: usize, ctx: &mut Ctx<'_>) {
        if index >= self.tabs.len() {
            return;
        }
        if self.tabs.len() == 1 {
            if let Some(s) = self.sender.take() {
                let _ = s.send(SessionCommand::Shutdown);
            }
            self.snapshot = None;
            self.snapshot_slot = None;
            self.sel = None;
            self.quick = None;
            self.preedit = None;
            self.tab_title = "shell".into();
            // Keep the portal binding alive. Start a fresh shell only on
            // the next explicit show, so an exiting shell cannot respawn-loop.
            self.hide(ctx);
            return;
        }
        if index == self.active_tab {
            let next = if index == 0 { 1 } else { index - 1 };
            self.switch_tab(next, ctx);
        }
        if let Some(tab) = self.tabs.remove(index) {
            let _ = tab.sender.send(SessionCommand::Shutdown);
        }
        if self.active_tab > index {
            self.active_tab -= 1;
        }
        ctx.request_redraw();
    }

    fn chrome_items(&self, cols: usize) -> Vec<crate::chrome::Item> {
        let titles: Vec<String> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| {
                if i == self.active_tab {
                    self.tab_title.clone()
                } else {
                    t.as_ref().map(|t| t.title.clone()).unwrap_or_default()
                }
            })
            .collect();
        crate::chrome::layout(
            cols,
            &titles,
            self.active_tab,
            self.settings_open,
            &self.config,
        )
    }

    fn save_appearance(&mut self, ctx: &mut Ctx<'_>, previous: &Config) {
        if let Err(e) = self.config.save_appearance_delta(previous) {
            self.status(format!("设置已应用但保存失败: {e}"), ctx);
        }
    }

    fn refresh_appearance(&mut self, ctx: &mut Ctx<'_>, previous: &Config) {
        if let Some(r) = &mut self.renderer {
            r.update_font(&self.config, ctx.scale_factor());
            r.update_colors(&self.config);
            let (w, h) = r.surface_size();
            self.relayout(ctx, w, h, true);
        }
        self.save_appearance(ctx, previous);
        ctx.request_redraw();
    }

    fn chrome_action(&mut self, action: crate::chrome::Action, ctx: &mut Ctx<'_>) {
        use crate::chrome::{Action, Field};
        match action {
            Action::Tab(i) => self.switch_tab(i, ctx),
            Action::Close(i) => self.close_tab(i, ctx),
            Action::New => self.new_tab(ctx),
            Action::Hide => self.hide(ctx),
            Action::Maximize => {
                self.maximized = !self.maximized;
                self.request_layer_height(ctx);
            }
            Action::CloseWindow => ctx.exit(),
            Action::Hotkeys => {
                self.settings_open = false;
                self.status(
                    format!("{} | Desktop command: tay toggle", self.hotkey_status),
                    ctx,
                );
                if std::env::var("XDG_CURRENT_DESKTOP")
                    .is_ok_and(|d| d.to_ascii_lowercase().contains("kde"))
                    && let Err(e) = std::process::Command::new("kcmshell6")
                        .arg("kcm_keys")
                        .spawn()
                {
                    self.status(format!("无法打开快捷键设置: {e}; 请绑定 tay toggle"), ctx);
                }
            }
            Action::Settings => {
                self.settings_open = !self.settings_open;
                self.editing = None;
            }
            Action::Smaller | Action::Larger => {
                let previous = self.config.clone();
                self.config.font.size = (self.config.font.size
                    + if action == Action::Larger { 1.0 } else { -1.0 })
                .clamp(6.0, 48.0);
                self.refresh_appearance(ctx, &previous);
            }
            Action::Edit(field) => {
                let value = match field {
                    Field::Font => self.config.font.family.clone(),
                    Field::Background => format!(
                        "#{:02x}{:02x}{:02x}",
                        self.config.colors.background.0,
                        self.config.colors.background.1,
                        self.config.colors.background.2
                    ),
                    Field::Foreground => format!(
                        "#{:02x}{:02x}{:02x}",
                        self.config.colors.foreground.0,
                        self.config.colors.foreground.1,
                        self.config.colors.foreground.2
                    ),
                };
                self.editing = Some((field, value));
            }
        }
        ctx.request_redraw();
    }

    fn apply_edit(&mut self, ctx: &mut Ctx<'_>) {
        use crate::chrome::Field;
        if let Some((field, value)) = self.editing.take() {
            let previous = self.config.clone();
            if field == Field::Font {
                if value.trim().is_empty() {
                    self.status("字体名称不能为空", ctx);
                    self.editing = Some((field, value));
                    return;
                }
                self.config.font.family = value.trim().into();
            } else {
                let Some(color) = Rgb::from_hex(&value) else {
                    self.status("请输入 #RRGGBB 颜色", ctx);
                    self.editing = Some((field, value));
                    return;
                };
                if field == Field::Background {
                    self.config.colors.background = color;
                } else {
                    self.config.colors.foreground = color;
                }
            }
            self.refresh_appearance(ctx, &previous);
        }
    }

    /// can_create_surfaces:建 GPU → 终端 → PTY 线程 → 定尺寸。
    fn init(&mut self, ctx: &mut Ctx<'_>) {
        let result = (|| -> Result<(), String> {
            // layer 高度 = 输出逻辑高度 × height_percent。
            let (out_w, out_h) = ctx.output_logical_size().unwrap_or((1920, 1080));
            let h = (out_h
                * if self.maximized {
                    100
                } else {
                    self.config.height_percent
                }
                / 100)
                .max(200);
            self.layer_h = h as i32;
            log::info!("layer 高度 {h}px(输出 {out_w}×{out_h})");
            ctx.set_size(0, h);

            // 渲染器 + GPU surface(输出回退重建时复用此函数)。
            let (cols, rows) = self.create_renderer(ctx)?;

            let tab = self.start_session(cols, rows)?;
            self.install_session(tab);

            self.set_ime_area(ctx, (0, 0));
            // 初始 margin 对齐(WindowState 建面时用 -2000 占位防闪屏)。
            self.vis.set_layer_h(self.layer_h);
            ctx.set_top_margin(-self.layer_h);
            ctx.request_redraw();

            // PTY 已 fork:现在才允许起多线程(IPC accept / portal)。
            if let Some(listener) = self.ipc_listener.take() {
                crate::ipc::accept_loop(listener, self.ipc_tx.clone(), self.wake.clone());
            }
            if self.shot.is_none() && self.bench.is_none() {
                crate::hotkey::spawn(
                    self.hotkey_tx.clone(),
                    self.wake.clone(),
                    self.config.global_shortcut.clone(),
                );
            }
            self.threads_started = true;

            Ok(())
        })();
        if let Err(e) = result {
            log::error!("初始化失败: {e}");
            ctx.exit();
        }
    }

    /// 当前光标位置(显示坐标;唯一来源 = 会话快照)。
    fn cursor_pos(&self) -> (usize, usize) {
        let Some(snap) = &self.snapshot else {
            return (0, 0);
        };
        self.view
            .cursor_position(snap.cursor, snap.dims.0)
            .map(|p| (p.col, p.row))
            .unwrap_or((0, 0))
    }

    fn set_ime_area(&self, ctx: &mut Ctx<'_>, (col, row): (usize, usize)) {
        if let Some(r) = &self.renderer {
            let (cw, ch) = r.cell_size();
            let pad = r.padding();
            ctx.set_ime_cursor_area(
                col as f32 * cw + pad,
                (row + crate::chrome::ROWS) as f32 * ch + pad,
                cw,
                ch,
            );
        }
    }

    /// 按表面尺寸重算网格与 PTY 尺寸。
    /// `force` = padding 等热重载触发的重算(表面尺寸未变也要算);
    /// reflow 使块/图像锚点失效(下一条 prompt 重建)。
    fn relayout(&mut self, ctx: &mut Ctx<'_>, width: u32, height: u32, force: bool) {
        let Some(r) = &mut self.renderer else {
            return;
        };
        if !force && r.surface_size() == (width, height) {
            return;
        }
        if r.surface_size() != (width, height) {
            r.resize(width, height);
        }
        let (cw, ch) = r.cell_size();
        let pad = r.padding();
        let cols = ((width as f32 - 2.0 * pad) / cw).floor().max(2.0) as usize;
        let rows = (((height as f32 - 2.0 * pad) / ch).floor() as usize)
            .saturating_sub(crate::chrome::ROWS)
            .max(1);
        if Some((cols, rows)) != self.dims {
            log::info!("网格调整 {cols}×{rows}");
            self.dims = Some((cols, rows));
            // 行号重排:显示坐标选区/QuickSelect 同属旧布局
            // (块/图像失效归会话线程,随 Resize 命令同批发布)。
            self.sel = None;
            self.quick = None;
            // 终端内核 resize 归会话线程(架构 §3):PTY 与网格一并更新。
            if let Some(sender) = &self.sender {
                let _ = sender.send(SessionCommand::Resize(WindowSize {
                    num_lines: rows as u16,
                    num_cols: cols as u16,
                    cell_width: cw.round() as u16,
                    cell_height: ch.round() as u16,
                }));
            }
        }
        // IME 光标矩形按新单元格尺寸重设(热重载 pad 后必走这里)。
        let pos = self.cursor_pos();
        self.set_ime_area(ctx, pos);
    }

    /// height_percent 热重载:重设 layer 高度并同步 VisibilityController。
    /// 走 sizing → configure → Resized 流程(与首次初始化同路径,
    /// 但 Resized 里按 sender 是否已存在区分「重算」与「重建终端」)。
    fn request_layer_height(&mut self, ctx: &mut Ctx<'_>) {
        let (_, out_h) = ctx.output_logical_size().unwrap_or((1920, 1080));
        let h = (out_h
            * if self.maximized {
                100
            } else {
                self.config.height_percent
            }
            / 100)
            .max(200);
        self.layer_h = h as i32;
        self.vis.set_layer_h(h as i32);
        log::info!(
            "呼出高度重设为 {h}px(height_percent={})",
            self.config.height_percent
        );
        ctx.set_size(0, h);
        // 隐藏时 margin 随高度同步提交:新高度比旧 -margin 深时,
        // 只改状态机内部值会让窗口底部露出(P2 修复)。
        ctx.set_top_margin(self.vis.margin());
        // 纯转换:重建需求不可被高度请求覆盖(第十三轮卡死回归)。
        self.gpu_phase = self.gpu_phase.on_height_request();
    }

    fn cell_at(&self, x: f64, y: f64) -> Option<(usize, usize)> {
        let (cw, ch) = self.renderer.as_ref()?.cell_size();
        let (cols, rows) = self.dims?;
        let pad = self.renderer.as_ref()?.padding() as f64;
        let c = ((x - pad) / cw as f64).floor();
        let r = ((y - pad) / ch as f64).floor() - crate::chrome::ROWS as f64;
        if c < 0.0 || r < 0.0 || c as usize >= cols || r as usize >= rows {
            None
        } else {
            Some((c as usize, r as usize))
        }
    }

    /// 从会话快照构建渲染网格(含命令块折叠重映射)。快照携带产生
    /// 它的 SessionMeta —— 网格与锚点必然同一版本(架构 §3);
    /// 尚未收到快照(会话刚启动)时渲染空白网格。
    fn build_grid(&mut self) -> tay_render::Grid {
        let snap = self.snapshot.clone();
        // 快照尺寸优先(会话版本),尚未有快照时回退到本地布局值。
        let (cols, rows) = snap
            .as_ref()
            .map(|s| s.dims)
            .unwrap_or(self.dims.unwrap_or((80, 24)));
        let (off, total_lines, meta) = match &snap {
            Some(s) => (s.display_offset as i32, s.total_lines, s.meta),
            None => (0, rows, Default::default()),
        };
        let blank = tay_render::Cell {
            c: ' ',
            fg: self.config.colors.foreground,
            bg: self.config.colors.background,
            bold: false,
            italic: false,
            underline: false,
            wide_spacer: false,
            zerowidth: None,
        };
        // —— 折叠重映射:可见网格行 → 显示行 ——
        let first = -off;
        let last = first + rows as i32 - 1;
        // 折叠区间(相邻提示符之间的输出行;P3c:存储逻辑在 blocks.rs)。
        // 锚点为绝对行,映射回 grid 行。
        let ranges: Vec<(i32, i32)> = snap
            .as_ref()
            .map(|s| s.blocks.fold_ranges(&self.folded))
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(a, b)| {
                // 唯一转换点(SessionMeta::resolve):早于历史淘汰的
                // 折叠区间直接丢弃(显示层不渲染失效锚点)。
                let ga = meta.resolve(a, total_lines, rows)?;
                let gb = meta.resolve(b, total_lines, rows)?;
                Some((ga, gb))
            })
            .collect();
        // 显示映射(架构 §2.3:TerminalView,view.rs;正向/反向
        // 转换唯一实现,绘制/命中/选区/光标共用)。
        let view = crate::view::ViewMap::new(first, last, &ranges).with_meta(meta);
        self.view = view.clone();
        let marker_fg = Rgb::new(0xc0, 0xa0, 0x40);
        let mut cells = vec![blank; rows * cols];
        for (row, e) in view.entries.iter().enumerate() {
            if e.marker {
                for c in 0..cols {
                    cells[row * cols + c] = tay_render::Cell {
                        c: ' ',
                        fg: marker_fg,
                        bg: Rgb::new(0x18, 0x1a, 0x20),
                        bold: false,
                        italic: false,
                        underline: false,
                        wide_spacer: false,
                        zerowidth: None,
                    };
                }
                for (i, ch) in e.text.chars().enumerate() {
                    if i < cols {
                        cells[row * cols + i].c = ch;
                    }
                }
            }
        }
        // 快照单元按网格行序(display_iter 序 = first..last);
        // 折叠占位行跳过(其网格行已在 map 中标记)。
        if let Some(s) = &snap
            && s.cells.len() == rows * cols
        {
            for (row, e) in view.entries.iter().enumerate() {
                if e.marker {
                    continue;
                }
                let gi = e.start - first;
                if gi < 0 || gi as usize >= rows {
                    continue;
                }
                let base = gi as usize * cols;
                for col in 0..cols {
                    let sc = &s.cells[base + col];
                    cells[row * cols + col] = tay_render::Cell {
                        c: sc.c,
                        fg: resolve(sc.fg, &s.colors, &self.config),
                        bg: resolve(sc.bg, &s.colors, &self.config),
                        bold: sc.flags.contains(Flags::BOLD),
                        italic: sc.flags.contains(Flags::ITALIC),
                        underline: sc.flags.contains(Flags::UNDERLINE),
                        wide_spacer: sc.flags.contains(Flags::WIDE_CHAR_SPACER),
                        zerowidth: sc.zerowidth.clone(),
                    };
                }
            }
        }
        let cursor_position = snap
            .as_ref()
            .and_then(|s| view.cursor_position(s.cursor, cols));
        let cursor_shape = match snap
            .as_ref()
            .filter(|_| cursor_position.is_some())
            .map(|s| s.cursor_shape)
        {
            Some(ACursorShape::Block) | Some(ACursorShape::HollowBlock) => {
                tay_render::CursorShape::Block
            }
            Some(ACursorShape::Underline) => tay_render::CursorShape::Underline,
            Some(ACursorShape::Beam) => tay_render::CursorShape::Beam,
            _ => tay_render::CursorShape::Hidden,
        };
        // 光标/选区经映射(折叠区间内的行落到占位行)。
        let to_display = |p: Point| -> (usize, usize) {
            let d = view.project(
                GridPoint {
                    line: p.line.0,
                    col: p.column.0 as u32,
                },
                cols,
            );
            (d.col, d.row)
        };
        // 选区渲染:鼠标拖选(UI 视图状态,选择时刻定格锚点)优先;
        // 无鼠标选区时渲染 vi 选区(会话侧,快照携带)。此前只渲染
        // 快照的 vi 选区 —— 重构回归:拖选看不见高亮但复制正常
        // (2026-10-19 修复,TAY_SEL 截图验色确认)。
        let selection = self
            .sel
            .and_then(|((sl, sc), (el, ec))| {
                let (a0, a1) = if sl <= el {
                    ((sl, sc), (el, ec))
                } else {
                    ((el, ec), (sl, sc))
                };
                let row0 = view.grid_to_row(a0.0.0 - meta.cum_scroll)?;
                let row1 = view.grid_to_row(a1.0.0 - meta.cum_scroll)?;
                let (r0, r1) = if row0 <= row1 {
                    (row0, row1)
                } else {
                    (row1, row0)
                };
                // 同一行的列序归一(反向拖选)。
                let (c0, c1) = if r0 == r1 && a0.1 > a1.1 {
                    (a1.1, a0.1)
                } else {
                    (a0.1, a1.1)
                };
                Some(tay_render::SelectionRange {
                    start: ((c0 as usize).min(cols - 1), r0),
                    end: ((c1 as usize).min(cols - 1), r1),
                })
            })
            .or_else(|| {
                snap.as_ref()
                    .and_then(|s| s.selection)
                    .map(|s| tay_render::SelectionRange {
                        start: to_display(s.start),
                        end: to_display(s.end),
                    })
            });
        // OSC 133 徽章:非零退出码显示在对应提示符行尾(块状态
        // 来自快照的 COW 共享引用,与网格同版本)。
        let badges = snap
            .as_ref()
            .map(|s| &*s.blocks)
            .into_iter()
            .flat_map(|blocks| blocks.badges())
            .filter_map(|(line, code)| {
                let gl = meta.resolve(*line, total_lines, rows)?;
                let row = view.grid_to_row(gl)?;
                Some(tay_render::Badge {
                    row,
                    text: format!(" ✗ {code} "),
                    color: Rgb::new(0xff, 0x5f, 0x5f),
                })
            })
            .collect();

        // kitty 图像:网格行 → 显示行(折叠区间内隐藏)。
        let (cw, ch) = self
            .renderer
            .as_ref()
            .map(|r| r.cell_size())
            .unwrap_or((10.0, 20.0));
        let images = snap
            .as_ref()
            .map(|s| &*s.images)
            .into_iter()
            .flatten()
            .filter_map(|e| {
                // 图像锚点同走唯一转换点;已被历史淘汰 → 不渲染。
                let gl = meta.resolve(HistoryAnchor(e.grid_line), total_lines, rows)?;
                let row = view.grid_to_row(gl)?;
                let cols = e
                    .img
                    .cols
                    .unwrap_or((e.img.w as f32 / cw).ceil() as u32)
                    .max(1);
                let rows = e
                    .img
                    .rows
                    .unwrap_or((e.img.h as f32 / ch).ceil() as u32)
                    .max(1);
                // 跨折叠区间 → 隐藏(修复:此前误把绝对行与
                // grid 相对区间比较,滚动后折叠隐藏失效)。
                if ranges
                    .iter()
                    .any(|(a, b)| gl + rows as i32 > *a && gl <= *b)
                {
                    return None;
                }
                Some(tay_render::RenderImage {
                    id: e.id,
                    row,
                    col: e.col as usize,
                    cols,
                    rows,
                    w: e.img.w,
                    h: e.img.h,
                    z: e.img.z,
                    rgba: e.img.rgba.clone(),
                })
            })
            .collect();

        // QuickSelect 覆盖层 + vi 搜索状态栏。
        let mut overlays: Vec<tay_render::Overlay> = self
            .quick
            .as_ref()
            .map(|q| {
                q.matches
                    .iter()
                    .map(|m| tay_render::Overlay {
                        row: m.row,
                        col: m.col,
                        text: m.label.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        if let Some(query) = &self.vi_search_query
            && rows > 0
        {
            overlays.push(tay_render::Overlay {
                row: rows - 1,
                col: 0,
                text: format!(" 搜索: {query} "),
            });
        }
        // 状态 toast(QuickSelect 反馈等):底部一行,约 1.2s 后消失。
        if let Some((msg, at)) = &self.status_msg
            && at.elapsed() < Duration::from_millis(1200)
            && rows > 0
        {
            overlays.push(tay_render::Overlay {
                row: rows - 1,
                col: 0,
                text: format!(" {msg} "),
            });
        }

        tay_render::Grid {
            cols,
            rows,
            cells,
            cursor: cursor_position.map(|p| (p.col, p.row)).unwrap_or((0, 0)),
            cursor_shape,
            selection,
            preedit: self.preedit.clone(),
            badges,
            overlays,
            images,
            window_icons: vec![],
        }
    }

    /// 折叠/展开光标所在命令块(Ctrl+Shift+H)。
    fn toggle_fold_at_cursor(&mut self, ctx: &mut Ctx<'_>) {
        let Some(snap) = &self.snapshot else { return };
        let line = snap.cursor.line;
        self.toggle_fold_line(line, ctx);
    }

    /// 折叠/展开包含网格行 `line` 的命令块(折叠按稳定 BlockId,
    /// 不依赖可能变化的行号 —— 架构 §3)。
    fn toggle_fold_line(&mut self, line: i32, ctx: &mut Ctx<'_>) {
        let Some(snap) = &self.snapshot else { return };
        let anchor = HistoryAnchor(line + snap.meta.cum_scroll);
        let Some(prompt) = snap.blocks.prompt_containing(anchor) else {
            return;
        };
        let Some(block) = snap.blocks.blocks().iter().find(|b| b.prompt == prompt) else {
            return;
        };
        let folded = if self.folded.remove(&block.id) {
            false
        } else {
            self.folded.insert(block.id);
            true
        };
        log::info!(
            "{}命令块 @ {}(#{})",
            if folded { "折叠" } else { "展开" },
            prompt.0,
            block.id.0
        );
        // 折叠改变布局:关闭 QuickSelect 与选区。
        self.quick = None;
        self.sel = None;
        ctx.request_redraw();
    }

    /// 显示单元 → 稳定选择点(网格行 + 与映射同源的 meta →
    /// 绝对锚点)。
    fn anchor_at(&self, cell: (usize, usize)) -> Option<(HistoryAnchor, u32)> {
        let line = self.view.row_to_grid(cell.1)?;
        if line < 0 {
            return None;
        }
        Some((
            HistoryAnchor(line + self.view.meta.cum_scroll),
            cell.0 as u32,
        ))
    }

    /// 点击是否落在退出码徽章上(显示点 → 网格点,坐标映射的
    /// 反向转换;命中返回该块的提示符所在网格点)。
    fn badge_line_at(&self, p: DisplayPoint) -> Option<GridPoint> {
        let grid_line = self.view.row_to_grid(p.row)?;
        if grid_line < 0 {
            return None;
        }
        let snap = self.snapshot.as_ref()?;
        let cum = snap.meta.cum_scroll;
        snap.blocks
            .nonzero_exit_at(HistoryAnchor(grid_line + cum))
            .and_then(|_| {
                let cols = self.dims.map(|(c, _)| c).unwrap_or(80);
                let badge_start = cols.saturating_sub(8);
                (p.col >= badge_start).then_some(GridPoint {
                    line: grid_line,
                    col: p.col as u32,
                })
            })
    }

    /// IME Commit → 会话输入(与 WEvent::Ime 臂同一实现,
    /// TAY_IME 验收钩子复用)。
    fn ime_commit(&mut self, text: String, ctx: &mut Ctx<'_>) {
        log::info!("IME 提交 {} 字节", text.len());
        if let Some(sender) = &self.sender
            && !sender.send(SessionCommand::Input(text.into_bytes()))
        {
            // 拒绝有可见反馈(统一预算满:终端积压)。
            self.status("输入被拒绝:终端积压,请稍后再试", ctx);
        }
        self.preedit = None;
        ctx.request_redraw();
    }

    /// vi 复制模式切换(Ctrl+Shift+Space;命令 → 会话)。
    fn toggle_vi(&mut self, ctx: &mut Ctx<'_>) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(SessionCommand::ToggleVi);
            self.vi_search_query = None;
            ctx.request_redraw();
        }
    }

    fn paste_clipboard(&mut self, ctx: &mut Ctx<'_>) {
        if let Some(text) = self.get_clipboard() {
            let bracketed = self
                .snapshot
                .as_ref()
                .is_some_and(|s| s.mode.contains(TermMode::BRACKETED_PASTE));
            let bytes = crate::input::paste_bytes(&text, bracketed);
            if let Some(sender) = &self.sender
                && !sender.send(SessionCommand::Input(bytes))
            {
                self.status("粘贴被拒绝:终端积压,请稍后再试", ctx);
            }
        }
    }

    /// kitty keyboard protocol 路径(§7 P1):应用启用 disambiguate 时
    /// 走 CSI u 编码;剪贴板快捷键仍由终端层拦截;flag 2 下抬起
    /// 事件也要上报。
    fn handle_kitty_key(
        &mut self,
        key: &tay_window::KeyPress,
        mods: tay_window::Mods,
        pressed: bool,
        kitty_flags: crate::keys::KittyFlags,
        ctx: &mut Ctx<'_>,
    ) {
        if pressed && crate::input::is_local(key.key, mods, "copy") {
            self.copy_selection();
            return;
        }
        if pressed && crate::input::is_local(key.key, mods, "paste") {
            self.paste_clipboard(ctx);
            return;
        }
        let event = if !pressed {
            crate::keys::KeyEventType::Release
        } else if key.repeat {
            crate::keys::KeyEventType::Repeat
        } else {
            crate::keys::KeyEventType::Press
        };
        if (pressed || kitty_flags.report_events)
            && let Some(bytes) = crate::keys::translate_kitty(key, mods, &kitty_flags, event)
            && let Some(sender) = &self.sender
        {
            if sender.send(SessionCommand::Input(bytes)) {
                if pressed && kitty_flags.report_events {
                    self.kitty_down.insert(key.raw_code, key.clone());
                }
            } else {
                self.status("输入被拒绝:终端积压,请稍后再试", ctx);
            }
        }
        ctx.request_redraw();
    }

    /// vi 模式按键 → 动作(仅消费,不发 PTY;全部经命令 → 会话)。
    fn handle_vi_key(&mut self, key: &tay_window::KeyPress, ctx: &mut Ctx<'_>) {
        use tay_window::KeyKind as K;
        use tay_window::NamedKey as N;

        // 搜索输入中:字符入查询,Enter 执行,Esc 取消。
        if self.vi_search_query.is_some() {
            match key.key {
                K::Named(N::Escape) => {
                    self.vi_search_query = None;
                    ctx.request_redraw();
                }
                K::Named(N::Enter) if self.vi_search_query.is_some() => {
                    self.vi_search_next(1, ctx);
                }
                K::Named(N::Backspace) => {
                    if let Some(q) = self.vi_search_query.as_mut() {
                        q.pop();
                    }
                    ctx.request_redraw();
                }
                K::Char(c) => {
                    if let Some(q) = self.vi_search_query.as_mut() {
                        q.push(c);
                    }
                    ctx.request_redraw();
                }
                _ => {}
            }
            return;
        }

        let motion = match key.key {
            K::Char('h') => Some(ViMotion::Left),
            K::Char('l') => Some(ViMotion::Right),
            K::Char('k') => Some(ViMotion::Up),
            K::Char('j') => Some(ViMotion::Down),
            K::Char('0') => Some(ViMotion::First),
            K::Char('$') => Some(ViMotion::Last),
            K::Char('^') => Some(ViMotion::FirstOccupied),
            K::Char('w') => Some(ViMotion::WordRight),
            K::Char('b') => Some(ViMotion::WordLeft),
            K::Char('e') => Some(ViMotion::WordRightEnd),
            K::Char('G') => Some(ViMotion::Low),
            K::Char('g') => Some(ViMotion::High),
            _ => None,
        };
        if let Some(m) = motion {
            if let Some(sender) = &self.sender {
                let _ = sender.send(SessionCommand::ViMotion(m));
            }
            ctx.request_redraw();
            return;
        }
        match key.key {
            // yank:复制选区并退出 vi(应答在 ProxyWake 排空 → 剪贴板)。
            K::Char('y') => {
                if let (Some(sender), Some(reply)) = (&self.sender, self.reply_tx.clone()) {
                    let _ = sender.send(SessionCommand::SelectionToString { reply });
                    let _ = sender.send(SessionCommand::ToggleVi);
                }
                ctx.request_redraw();
            }
            // 搜索。
            K::Char('/') => {
                self.vi_search_query = Some(String::new());
                ctx.request_redraw();
            }
            K::Char('n') => self.vi_search_next(1, ctx),
            K::Char('N') => self.vi_search_next(-1, ctx),
            // 退出。
            K::Named(N::Escape) | K::Char('q') => {
                if let Some(sender) = &self.sender {
                    let _ = sender.send(SessionCommand::ToggleVi);
                }
                ctx.request_redraw();
            }
            _ => {}
        }
    }

    /// 下一个/上一个搜索命中(dir = 1/-1;搜索在会话侧执行)。
    fn vi_search_next(&mut self, dir: i32, ctx: &mut Ctx<'_>) {
        let Some(query) = self.vi_search_query.clone() else {
            return;
        };
        if let (Some(sender), Some(reply)) = (&self.sender, self.reply_tx.clone()) {
            let _ = sender.send(SessionCommand::Search { query, dir, reply });
        }
        ctx.request_redraw();
    }

    /// 跳转到上一条/下一条提示符(DESIGN §8.4「跳转到上一条提示符」)。
    /// 视口顶来自快照;滚动定位在会话侧解析 + 世代校验(淘汰/过期
    /// 目标 no-op)。
    fn jump_prompt(&mut self, dir: i32) {
        let Some(snap) = &self.snapshot else { return };
        let top = HistoryAnchor(-(snap.display_offset as i32) + snap.meta.cum_scroll);
        let Some(blocks) = self.snapshot.as_ref().map(|s| s.blocks.clone()) else {
            return;
        };
        let target = if dir < 0 {
            blocks.prompt_before(top)
        } else {
            blocks.prompt_after(top)
        };
        if let Some(line) = target
            && let Some(sender) = &self.sender
        {
            // 携带来源世代:执行前清屏/resize/屏幕切换 → 会话拒绝。
            let epoch = snap.meta.invalidate_epoch;
            let _ = sender.send(SessionCommand::ScrollToAnchor {
                anchor: line,
                epoch,
            });
        }
    }

    /// 复制上一条命令的输出(§8.4「按块复制」)。范围在会话侧解析
    /// 并裁剪到存活历史;请求携带来源世代,执行前校验(应答携带
    /// 版本与过期标记,ProxyWake 排空 → 剪贴板/轻量反馈)。
    fn copy_command_output(&mut self) {
        let Some(snap) = self.snapshot.as_ref() else {
            return;
        };
        let Some((from_line, end_line, end_col)) = snap.blocks.last_output_range() else {
            return;
        };
        if let (Some(sender), Some(reply)) = (&self.sender, self.reply_tx.clone()) {
            let _ = sender.send(SessionCommand::CopyAnchors {
                from_line,
                from_col: 0,
                to_line: end_line,
                to_col: end_col,
                epoch: snap.meta.invalidate_epoch,
                reply,
            });
        }
    }

    /// 写系统剪贴板(持久实例;失败仅告警一次,不打断操作)。
    fn set_clipboard(&mut self, text: String) {
        match &mut self.clipboard {
            Some(cb) => {
                if let Err(e) = cb.set_text(text) {
                    log::warn!("写剪贴板失败: {e}");
                }
            }
            None => log::warn!("剪贴板不可用(无会话?)"),
        }
    }

    /// 读系统剪贴板。
    fn get_clipboard(&mut self) -> Option<String> {
        self.clipboard.as_mut()?.get_text().ok()
    }

    /// 滚轮滚动(ScrollDisplay 命令;TAY_SCROLL 钩子共用)。
    fn scroll_delta(&mut self, delta: i32) {
        if delta != 0
            && let Some(sender) = &self.sender
        {
            let _ = sender.send(SessionCommand::ScrollDisplay(Scroll::Delta(delta)));
        }
    }

    /// 状态 toast:底部一行反馈(约 1.2s 后消失),同时记日志。
    fn status(&mut self, msg: impl Into<String>, ctx: &mut Ctx<'_>) {
        let msg = msg.into();
        log::info!("{msg}");
        self.status_msg = Some((msg, Instant::now()));
        // 到期后醒来重绘一帧清除 toast。
        ctx.request_wake_at(Instant::now() + Duration::from_millis(1200));
        ctx.request_redraw();
    }

    /// 选区复制:优先自己的鼠标选区,其次 vi 选区(会话侧应答)。
    fn copy_selection(&mut self) {
        let (Some(sender), Some(reply)) = (&self.sender, self.reply_tx.clone()) else {
            return;
        };
        if let Some((start, end)) = self.sel {
            // 稳定锚点直发(P1):选择时刻已定格,滚屏不改变指向的
            // 文本;请求携带来源世代 —— 会话执行前校验,过期
            // (清屏/resize/屏幕切换)明确返回,不猜坐标(第八轮)。
            let (s, e) = if start.0 <= end.0 {
                (start, end)
            } else {
                (end, start)
            };
            let _ = sender.send(SessionCommand::CopyAnchors {
                from_line: s.0,
                from_col: s.1,
                to_line: e.0,
                to_col: Some(e.1),
                epoch: self.sel_epoch,
                reply,
            });
        } else {
            let _ = sender.send(SessionCommand::SelectionToString { reply });
        }
    }

    fn render(&mut self, ctx: &mut Ctx<'_>) {
        let t0 = Instant::now();
        let Some(_) = self.renderer.as_ref() else {
            return;
        };
        self.frames += 1;

        // TAY_SLOWUI:第 15..80 帧渲染前人为延迟 ~30ms(慢 UI 模拟):
        // 期间 PTY 输出/输入照常流动 —— 验证快照单槽覆盖(UI 追
        // 上最新状态)、输入预算不因 UI 卡顿失真、帧循环不触发
        // stalled 看门狗(30ms < 50ms 兜底)。
        if self.slowui_at_start && (15..80).contains(&self.frames) {
            std::thread::sleep(Duration::from_millis(30));
        }

        // 热呼出延迟:呼出指令 → 首帧。
        if let Some(at) = self.show_at.take() {
            self.shown_once = true;
            log::info!("热呼出 {}ms", at.elapsed().as_millis());
        }

        // TAY_IME:第 11 帧注入 IME Commit(中文输入链路验收:
        // Commit → SessionCommand::Input → PTY → shell 回显 →
        // 快照 → 渲染)。
        if let Some(text) = self.ime_at_start.clone()
            && self.frames == 11
        {
            self.ime_at_start = None;
            self.ime_commit(text, ctx);
        }

        // TAY_REBIND:第 15 帧强制 surface 重建(输出回退路径验收;
        // 单显示器机器无法拔线,与 output_destroyed 同一入口)。
        if self.rebind_at_start && self.frames == 15 {
            self.rebind_at_start = false;
            log::info!("TAY_REBIND:强制 surface 重建(验收)");
            ctx.force_rebind();
        }

        // TAY_SCROLL:第 16 帧注入滚轮滚动(ScrollDisplay 命令
        // 链路实机验证)。
        if self.scroll_at_start && self.frames == 16 {
            self.scroll_at_start = false;
            log::info!("TAY_SCROLL:注入滚动(验收)");
            self.scroll_delta(2);
        }

        // TAY_SEL:第 17 帧合成拖选并复制(选择时刻定格锚点 →
        // CopyAnchors → 会话解析 → 应答,完整链路实机验证)。
        if self.sel_at_start && self.frames == 17 {
            self.sel_at_start = false;
            log::info!("TAY_SEL:合成拖选并复制(验收)");
            if let (Some(a0), Some(a1)) = (self.anchor_at((0, 0)), self.anchor_at((20, 0))) {
                self.sel = Some((a0, a1));
                self.sel_epoch = self.view.meta.invalidate_epoch;
                self.copy_selection();
            }
        }

        // TAY_PASTE:第 18/19 帧注入两次 300 KiB 大段粘贴(统一输入
        // 预算端到端:第一次验证空队列接受 + 完整交付;第二次在
        // 第一次尚未写完(PTY 缓冲满,待写队列积压)时触发预算
        // 拒绝 + 可见反馈 —— 验收「已接受不丢失,超限显式拒绝整块
        // 且用户可见」)。配 TAY_SHELL 慢消费脚本(如
        // `bash -c "stty raw -echo; sleep 60"`)确定性复现。
        if self.paste_at_start && (self.frames == 18 || self.frames == 19) {
            let second = self.frames == 19;
            if second {
                self.paste_at_start = false;
            }
            let paste = vec![if second { b'b' } else { b'a' }; 300 * 1024];
            if let Some(sender) = &self.sender
                && !sender.send(SessionCommand::Input(paste))
            {
                log::error!(
                    "TAY_PASTE:{}粘贴被拒绝(统一预算已满,预期)",
                    if second { "第二次" } else { "第一次" }
                );
                self.status("粘贴被拒绝:终端积压,请稍后再试", ctx);
            } else if second {
                log::info!("TAY_PASTE:第二次粘贴也接受(第一次已写掉,正常)");
            } else {
                log::info!("TAY_PASTE:300 KiB 粘贴已接受");
            }
        }

        // TAY_FOLD:第 14 帧自动折叠第一条命令输出(验证用)。
        if self.fold_at_start && self.frames == 14 {
            self.fold_at_start = false;
            if let Some(block) = self
                .snapshot
                .as_ref()
                .and_then(|s| s.blocks.blocks().first())
            {
                self.folded.insert(block.id);
                log::info!("折叠验证:块 #{} @ {}", block.id.0, block.prompt.0);
            }
        }

        // TAY_VI:第 13 帧自动进入 vi 模式(验证用)。
        if self.vi_at_start && self.frames == 13 {
            self.vi_at_start = false;
            if let Some(sender) = &self.sender {
                let _ = sender.send(SessionCommand::ToggleVi);
                log::info!("已进入 vi 复制模式(验证)");
            }
        }

        // TAY_QUICKSELECT:第 12 帧自动触发(验证用)。
        if self.quickselect_at_start && self.frames == 12 {
            self.quickselect_at_start = false;
            let grid = self.build_grid();
            let matches = crate::quickselect::build(&grid);
            log::info!("QuickSelect: {} 个匹配项", matches.len());
            self.quick = Some(QuickState {
                matches,
                typed: String::new(),
            });
        }

        // TAY_EXEC:第 10 帧执行命令(等 shell 就绪)。
        if !self.exec_sent && self.frames >= 10 {
            self.exec_sent = true;
            if let (Some(sender), Some(cmd)) = (&self.sender, &self.exec) {
                log::info!("执行: {cmd}");
                let _ = sender.send(SessionCommand::Input(format!("{cmd}\n").into_bytes()));
            }
        }

        // 基准:启动后第 20 帧灌入 3000 行输出。
        if let Some(bench) = self.bench
            && !self.bench_started
            && self.frames >= 20
        {
            self.bench_started = true;
            log::info!("基准:灌入 3000 行输出");
            if let Some(sender) = &self.sender {
                let cmd = "i=0; while [ $i -lt 3000 ]; do echo $i; i=$((i+1)); done\n";
                let _ = sender.send(SessionCommand::Input(cmd.as_bytes().to_vec()));
            }
            let _ = bench;
        }
        // P2b:完全收起(滑出屏幕)时跳过 GPU 渲染 —— 网格构建、纹理
        // acquire/present 全部省掉;内容在下次呼出时按当前网格重绘,
        // 不丢任何东西。截图/基准模式需要连续帧,不受此限制。
        let hidden_idle = self.vis.is_hidden()
            && !self.renderer.as_ref().is_some_and(|r| r.shot_pending())
            && self
                .bench
                .is_none_or(|n| (self.bench_times.len() as u32) >= n);
        if hidden_idle {
            return;
        }
        let mut grid = self.build_grid();
        if grid.cursor_shape != tay_render::CursorShape::Hidden {
            self.set_ime_area(ctx, grid.cursor);
        }
        let items = self.chrome_items(grid.cols);
        let message = if let Some((field, value)) = &self.editing {
            format!("Edit {field:?}: {value}_  Enter=save  Esc=cancel  Ctrl+A=replace")
        } else if let Some(action) = self.chrome_hover.filter(|a| {
            matches!(
                a,
                crate::chrome::Action::Hide
                    | crate::chrome::Action::Maximize
                    | crate::chrome::Action::CloseWindow
            )
        }) {
            match action {
                crate::chrome::Action::Hide => "Minimize: hide window, keep sessions".into(),
                crate::chrome::Action::Maximize => "Maximize / restore height".into(),
                _ => "Close window: end all sessions".into(),
            }
        } else if self.settings_open {
            String::new()
        } else {
            format!(
                "{} | {}: new | {}: switch | {}: settings",
                self.hotkey_status,
                self.config.binding_hint("new-tab"),
                self.config.binding_hint("next-tab"),
                self.config.binding_hint("settings")
            )
        };
        crate::chrome::decorate(
            &mut grid,
            &items,
            &message,
            &self.config,
            self.chrome_hover,
            self.maximized,
        );
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
                    } else {
                        // present 完成后再请求下一帧回调:带 buffer 的
                        // commit 才会被 KWin 响应(pre_present 的空 commit
                        // 注册不触发 done,会导致帧循环停摆)。
                        ctx.arm_frame_after_present();
                    }
                }
                wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                    let (w, h) = renderer.surface_size();
                    renderer.resize(w, h);
                }
                wgpu::CurrentSurfaceTexture::Timeout
                | wgpu::CurrentSurfaceTexture::Occluded
                | wgpu::CurrentSurfaceTexture::Validation => {}
            }
        }

        // TAY_SHOT / TAY_BENCH:连续重绘,驱动帧计数。
        let continuous = self.renderer.as_ref().is_some_and(|r| r.shot_pending())
            || self
                .bench
                .is_some_and(|n| (self.bench_times.len() as u32) < n);
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
            if let Some(n) = self.bench
                && self.bench_times.len() as u32 >= n
            {
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

impl Drop for TayApp {
    fn drop(&mut self) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(SessionCommand::Shutdown);
        }
        for tab in self.tabs.iter().flatten() {
            let _ = tab.sender.send(SessionCommand::Shutdown);
        }
    }
}

impl Handler for TayApp {
    fn on_event(&mut self, ctx: &mut Ctx<'_>, event: WEvent) {
        if let WEvent::PointerMoved { x, y } = &event
            && let Some(r) = &self.renderer
        {
            let (cw, ch) = r.cell_size();
            let pad = r.padding() as f64;
            let col = ((*x - pad) / cw as f64).floor();
            let row = ((*y - pad) / ch as f64).floor();
            let hovered = if col >= 0.0 && row >= 0.0 && row < crate::chrome::ROWS as f64 {
                crate::chrome::hit(
                    &self.chrome_items(self.dims.map(|d| d.0).unwrap_or(80)),
                    row as usize,
                    col as usize,
                )
            } else {
                None
            };
            ctx.set_pointer_over_control(hovered.is_some());
            if hovered != self.chrome_hover {
                self.chrome_hover = hovered;
                ctx.request_redraw();
            }
        }
        if let WEvent::PointerButton {
            button: MouseButtonIdx::Left,
            pressed,
            x,
            y,
        } = &event
            && let Some(r) = &self.renderer
        {
            let (cw, ch) = r.cell_size();
            let pad = r.padding() as f64;
            let row = ((*y - pad) / ch as f64).floor();
            let col = ((*x - pad) / cw as f64).floor();
            if row >= 0.0 && row < (crate::chrome::ROWS as f64) {
                if *pressed && col >= 0.0 && !(row as usize == 2 && self.editing.is_some()) {
                    let items = self.chrome_items(self.dims.map(|d| d.0).unwrap_or(80));
                    if let Some(a) = crate::chrome::hit(&items, row as usize, col as usize) {
                        self.chrome_action(a, ctx);
                    }
                }
                return;
            }
        }
        match event {
            WEvent::CanCreateSurfaces
                // 第一 configure 后:按输出高度重设 layer 尺寸,
                // 等待 Resized 确认后一次性初始化渲染器(只 configure 一次,
                // 规避 Mesa WSI 二次 swapchain 重建的段错误)。
                if self.sender.is_none() => {
                    let (_, out_h) = ctx.output_logical_size().unwrap_or((1920, 1080));
                    let h = (out_h * if self.maximized {100}else{self.config.height_percent} / 100).max(200);
                    self.layer_h = h as i32;
                    log::info!("layer 高度 {h}px(输出 {out_h}px 高)");
                    ctx.set_size(0, h);
                    self.gpu_phase = GpuPhase::AwaitingConfigure { rebuild: false };
                }

            WEvent::Resized { width: _, height } => {
                if matches!(self.gpu_phase, GpuPhase::AwaitingConfigure { .. })
                    && height as i32 >= self.layer_h - 1
                    && height as i32 <= self.layer_h + 1
                {
                    let rebuild = self.gpu_phase.on_resized_handshake();
                    self.gpu_phase = GpuPhase::Ready;
                    if self.sender.is_none() {
                        // 首次初始化:建终端/PTY/渲染器。
                        self.init(ctx);
                        // M1 常驻模型:默认收起;TAY_START_VISIBLE / 配置可让启动即呼出。
                        if !self.config.start_hidden {
                            self.show(ctx);
                        }
                    } else if rebuild {
                        // 输出回退:新 surface 的 configure 握手完成,
                        // 重建渲染器(终端/PTY 不动),重新对齐 margin。
                        log::info!("configure 握手完成,重建渲染器");
                        if let Err(e) = self.create_renderer(ctx) {
                            log::error!("重建渲染器失败: {e}");
                            ctx.exit();
                            return;
                        }
                        // 网格尺寸可能变化:PTY 同步(内核 resize 归会话)。
                        if let Some((cols, rows)) = self.dims
                            && let Some(sender) = &self.sender
                        {
                            {
                                let (cw, ch) = self
                                    .renderer
                                    .as_ref()
                                    .map(|r| r.cell_size())
                                    .unwrap_or((8.0, 16.0));
                                let _ = sender.send(SessionCommand::Resize(WindowSize {
                                    num_lines: rows as u16,
                                    num_cols: cols as u16,
                                    cell_width: cw.round() as u16,
                                    cell_height: ch.round() as u16,
                                }));
                            }
                        }
                        // 新 surface 上 margin 为全离屏,重新对齐。
                        let m = self.vis.margin();
                        ctx.set_top_margin(m);
                        ctx.request_redraw();
                    } else {
                        // 热重载高度:终端已存在,只重算网格。
                        let (w,h)=ctx.surface_size();
                        self.relayout(ctx, w, h, false);
                        ctx.request_redraw();
                    }
                    return;
                }
                // 表面尺寸变化 → 网格重算(force=false:同尺寸跳过)。
                let (w,h)=ctx.surface_size();
                        self.relayout(ctx, w, h, false);
                ctx.request_redraw();
            }

            WEvent::ScaleFactor(scale) => {
                if let Some(r) = &mut self.renderer {
                    r.update_font(&self.config, scale);
                    r.update_padding(self.config.padding * scale as f32);
                    let (w, h) = ctx.surface_size();
                    self.relayout(ctx, w, h, true);
                    ctx.request_redraw();
                }
            }

            WEvent::Focused(true) => {ctx.request_keyboard(true,false);}
            WEvent::Focused(focused)
                // 失焦自动隐藏(DESIGN §6.3:Wayland 上即 keyboard leave);
                // `tay pin` 临时暂停。
                if !focused
                    && self.config.hide_on_focus_out
                    && !self.vis.pinned()
                    && !self.vis.is_hidden()
                => {
                    self.hide(ctx);
                }

            WEvent::Keyboard { key, pressed, mods } => {
                if !pressed {
                    if let Some(original) = self.kitty_down.remove(&key.raw_code) {
                        let mode = self.snapshot.as_ref().map(|s|s.mode).unwrap_or_default();
                        let flags = crate::keys::KittyFlags {
                            disambiguate: mode.contains(TermMode::DISAMBIGUATE_ESC_CODES),
                            report_events: mode.contains(TermMode::REPORT_EVENT_TYPES),
                            report_alternate: mode.contains(TermMode::REPORT_ALTERNATE_KEYS),
                            report_all: mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC),
                            report_associated: false,
                        };
                        self.handle_kitty_key(&original, mods, false, flags, ctx);
                    }
                    return;
                }
                if self.editing.is_some() {
                    if pressed {
                        use tay_window::NamedKey;
                        match key.key {
                            KeyKind::Named(NamedKey::Escape)=>self.editing=None,
                            KeyKind::Named(NamedKey::Enter)=>self.apply_edit(ctx),
                            KeyKind::Named(NamedKey::Backspace)=>{if let Some((_,v))=&mut self.editing{v.pop();}},
                            KeyKind::Char('a'|'A') if mods.ctrl=>{if let Some((_,v))=&mut self.editing{v.clear();}},
                            _ if !mods.ctrl && !mods.alt && !mods.meta=>{if let (Some((_,v)),Some(text))=(&mut self.editing,&key.text){v.push_str(text);}},
                            _=>{},
                        }
                        ctx.request_redraw();
                    }
                    return;
                }
                if !pressed {
                    return;
                }
                // 用户 Lua 键位(config.keys):优先级最高(覆盖内置
                // 快捷键与 QuickSelect 标签),但**只在按下时**触发 ——
                // release 也携带相同 mods,放在 pressed 守卫之前会
                // 双击执行(粘贴两次、maximize/settings 翻转两次净
                // 效果为零,2026-10-20 修复)。
                if let Some(action)=crate::input::configured_key_action(&self.config,&key,pressed,mods).map(str::to_owned) {
                    log::debug!("configured shortcut: {action}");
                    use crate::chrome::Action;
                    match action.as_str() {
                        "new-tab"=>self.new_tab(ctx), "close-tab"=>self.close_tab(self.active_tab,ctx),
                        "next-tab"=>self.switch_tab((self.active_tab+1)%self.tabs.len(),ctx),
                        "previous-tab"=>self.switch_tab((self.active_tab+self.tabs.len()-1)%self.tabs.len(),ctx),
                        "copy"=>self.copy_selection(), "paste"=>self.paste_clipboard(ctx),
                        "hide"=>self.hide(ctx), "maximize"=>self.chrome_action(Action::Maximize,ctx),
                        "settings"=>self.chrome_action(Action::Settings,ctx),
                        "font-increase"=>self.chrome_action(Action::Larger,ctx),
                        "font-decrease"=>self.chrome_action(Action::Smaller,ctx),
                        "font-reset"=>{match Config::base_font_size() { Ok(size)=>{let previous=self.config.clone();self.config.font.size=size;self.refresh_appearance(ctx,&previous);}, Err(e)=>self.status(format!("无法重置字号: {e}"),ctx) }},
                        "none"=>{},
                        other=>if let Some(n)=other.strip_prefix("tab-").and_then(|n|n.parse::<usize>().ok()){self.switch_tab(n-1,ctx);},
                    }
                    return;
                }

                // QuickSelect 激活:消费按键(标签 / Esc 取消)。
                if self.quick.is_some() {
                    match key.key {
                        KeyKind::Named(tay_window::NamedKey::Escape) => {
                            self.quick = None;
                            ctx.request_redraw();
                            return;
                        }
                        KeyKind::Char(c) => {
                            // Shift+标签 = 大写字符,仍按小写标签匹配;
                            // Shift 本身作为"同时打开"修饰(P2e 分离打开动作)。
                            let label_char = c.to_ascii_lowercase();
                            if !label_char.is_ascii_lowercase() {
                                self.quick = None;
                                ctx.request_redraw();
                                return;
                            }
                            let fire = {
                                let q = self.quick.as_mut().unwrap();
                                q.typed.push(label_char);
                                // 前缀无匹配 → 取消。
                                let any_prefix =
                                    q.matches.iter().any(|m| m.label.starts_with(&q.typed));
                                if !any_prefix {
                                    self.quick = None;
                                    return;
                                }
                                q.resolve().is_some()
                            };
                            if fire {
                                let q = self.quick.take().unwrap();
                                if let Some(m) = q.resolve() {
                                    let open = mods.shift && m.is_url;
                                    crate::quickselect::activate(m, open, &mut self.clipboard);
                                    // 反馈:状态 toast + 日志。
                                    let what = if open {
                                        "已复制并打开"
                                    } else {
                                        "已复制"
                                    };
                                    self.status(format!("{what} {}", m.text), ctx);
                                }
                            }
                            ctx.request_redraw();
                            return;
                        }
                        _ => {
                            // 其他按键取消。
                            self.quick = None;
                            ctx.request_redraw();
                            return;
                        }
                    }
                }
                // InputRouter(架构 §2.4):路由优先级集中在
                // input::classify(第一档 → vi → kitty),本地动作表
                // 单一来源 input::is_local;见 crates/tayd/src/input.rs。
                // 模式来自会话快照(快照与内容同版本;按键翻译用
                // 的快照滞后一个输出批次,与 PTY 往返同级,可接受)。
                let mode = self
                    .snapshot
                    .as_ref()
                    .map(|s| s.mode)
                    .unwrap_or_default();
                let in_vi = mode.contains(TermMode::VI);
                let kitty_flags = crate::keys::KittyFlags {
                    disambiguate: mode.contains(TermMode::DISAMBIGUATE_ESC_CODES),
                    report_events: mode.contains(TermMode::REPORT_EVENT_TYPES),
                    report_alternate: mode.contains(TermMode::REPORT_ALTERNATE_KEYS),
                    report_all: mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC),
                    report_associated: mode.contains(TermMode::REPORT_ASSOCIATED_TEXT),
                };
                match crate::input::classify(
                    key.key,
                    mods,
                    false,
                    in_vi,
                    kitty_flags.disambiguate || kitty_flags.report_events || kitty_flags.report_alternate || kitty_flags.report_all || kitty_flags.report_associated,
                ) {
                    crate::input::KeyRoute::Local("fold") => {
                        self.toggle_fold_at_cursor(ctx);
                        return;
                    }
                    crate::input::KeyRoute::Local("vi-toggle") => {
                        self.toggle_vi(ctx);
                        return;
                    }
                    crate::input::KeyRoute::ViConsume => {
                        self.handle_vi_key(&key, ctx);
                        return;
                    }
                    crate::input::KeyRoute::KittyProtocol => {
                        self.handle_kitty_key(&key, mods, pressed, kitty_flags, ctx);
                        return;
                    }
                    _ => {}
                }

                // 触发 QuickSelect:Ctrl+Shift+F(第二档)。
                if crate::input::is_local(key.key, mods, "quickselect") {
                    let grid = self.build_grid();
                    let matches = crate::quickselect::build(&grid);
                    if matches.is_empty() {
                        log::info!("QuickSelect: 无匹配项");
                    } else {
                        log::info!("QuickSelect: {} 个匹配项,按键选择", matches.len());
                        self.quick = Some(QuickState {
                            matches,
                            typed: String::new(),
                        });
                    }
                    ctx.request_redraw();
                    return;
                }
                // 剪贴板/命令块快捷键(第二档;表单一来源 input.rs)。
                if mods.ctrl && mods.shift {
                    if crate::input::is_local(key.key, mods, "copy") {
                        self.copy_selection();
                        return;
                    }
                    if crate::input::is_local(key.key, mods, "paste") {
                        self.paste_clipboard(ctx);
                        return;
                    }
                    // 命令块:跳转提示符 / 复制上一条命令输出。
                    if crate::input::is_local(key.key, mods, "prev-prompt") {
                        self.jump_prompt(-1);
                        ctx.request_redraw();
                        return;
                    }
                    if crate::input::is_local(key.key, mods, "next-prompt") {
                        self.jump_prompt(1);
                        ctx.request_redraw();
                        return;
                    }
                    if crate::input::is_local(key.key, mods, "copy-block") {
                        self.copy_command_output();
                        return;
                    }
                }
                let app_cursor = self
                    .snapshot
                    .as_ref()
                    .is_some_and(|s| s.mode.contains(TermMode::APP_CURSOR));
                if let Some(bytes) = crate::keys::translate(&key, mods, app_cursor)
                    && let Some(sender) = &self.sender
                    && !sender.send(SessionCommand::Input(bytes))
                {
                    self.status("输入被拒绝:终端积压,请稍后再试", ctx);
                }
                let pos = self.cursor_pos();
                self.set_ime_area(ctx, pos);
                ctx.request_redraw();
            }

            WEvent::Ime(ime) => match ime {
                ImeEvent::Commit(text) => {if let Some((_,value))=&mut self.editing{value.push_str(&text);ctx.request_redraw();}else{self.ime_commit(text,ctx);}},
                ImeEvent::Preedit { text, cursor } => {
                    if text.is_empty() {
                        self.preedit = None;
                    } else {
                        let (col, row) = self.cursor_pos();
                        self.preedit = Some(tay_render::Preedit {
                            text,
                            col,
                            row,
                            cursor_bytes: cursor,
                        });
                        self.set_ime_area(ctx, (col, row));
                    }
                    ctx.request_redraw();
                }
                _ => {}
            },

            WEvent::PointerButton {
                button: MouseButtonIdx::Left,
                pressed,
                x,
                y,
            } => {
                if pressed {
                        // 点击徽章 → 折叠/展开该块。
                        if let Some(cell) = self.cell_at(x, y)
                            && let Some(line) = self.badge_line_at(DisplayPoint {
                                row: cell.1,
                                col: cell.0,
                            }) {
                                self.toggle_fold_line(line.line, ctx);
                                return;
                            }
                        self.dragging = true;
                        if let Some(cell) = self.cell_at(x, y)
                            && let Some(a) = self.anchor_at(cell)
                        {
                            self.sel = Some((a, a));
                            // 选择来源世代 = 当前显示映射的世代(复制
                            // 请求携带,会话执行前校验)。
                            self.sel_epoch = self.view.meta.invalidate_epoch;
                        }
                    } else {
                        self.dragging = false;
                        if let Some((s, e)) = self.sel
                            && s == e {
                                self.sel = None;
                            }
                    }
                    ctx.request_redraw();
                }

            WEvent::PointerMoved { x, y }
                if self.dragging => {
                    if let Some(cell) = self.cell_at(x, y)
                        && let Some(a) = self.anchor_at(cell)
                        && let Some((start, _)) = self.sel
                    {
                        self.sel = Some((start, a));
                    }
                    ctx.request_redraw();
                }

            WEvent::Scroll {
                lines_y, pixel_y, ..
            } => {
                let mut delta = lines_y as i32 * 3;
                if lines_y == 0.0 && pixel_y != 0.0 {
                    self.scroll_px += pixel_y;
                    let ch = self
                        .renderer
                        .as_ref()
                        .map(|r| r.cell_size().1)
                        .unwrap_or(20.0);
                    delta = (self.scroll_px / ch) as i32;
                    self.scroll_px -= delta as f32 * ch;
                }
                self.scroll_delta(delta);
            }

            WEvent::RedrawRequested => {
                self.render(ctx);
            }

            WEvent::ProxyWake => {
                let mut exited=Vec::new();
                for(i,tab)in self.tabs.iter_mut().enumerate(){if let Some(t)=tab{while let Ok(e)=t.rx.try_recv(){match e{Event::Title(title)=>t.title=title,Event::Exit|Event::ChildExit(_)=>{exited.push(i);break;},_=>{}}}}}
                for i in exited.into_iter().rev(){self.close_tab(i,ctx);}

                // 原子批次(架构 §7):快照与语义事件一起取出 ——
                // 网格、命令块、图像、清屏事件必然同源,不存在
                // 「旧网格配新事件」的版本竞争。
                let update = self
                    .snapshot_slot
                    .as_ref()
                    .and_then(|slot| slot.lock().unwrap().take());
                if let Some(update) = update {
                    // 网格失效世代变化 → 丢弃**坐标键控**的视图状态:
                    // 鼠标选区(历史锚点)与 QuickSelect(显示坐标)在
                    // 清屏/重置/resize/主备屏幕切换后已失效。
                    // 折叠状态按**稳定 BlockId** 键控(不随行号变化,
                    // 且 BlockId 单调不复用),由下方 live-retain 按
                    // 块存活裁剪 —— 不得在此清空:alt-screen 切换
                    // 推进世代但主屏块锚点保持有效(网格冻结),
                    // 清空会把进 vim 前用户的折叠全部弄丢(第十七轮
                    // 回归,已修复)。
                    if update.snapshot.meta.invalidate_epoch != self.seen_epoch {
                        self.seen_epoch = update.snapshot.meta.invalidate_epoch;
                        self.sel = None;
                        self.quick = None;
                    }
                    // 折叠块裁剪:历史淘汰掉的块 ID 从折叠集合移除。
                    let live: std::collections::BTreeSet<_> = update
                        .snapshot
                        .blocks
                        .blocks()
                        .iter()
                        .map(|b| b.id)
                        .collect();
                    self.folded.retain(|id| live.contains(id));
                    self.snapshot = Some(update.snapshot);
                }
                // 会话应答:复制/搜索回执(携带版本)。乱序/过期语义:
                // 复制文本在会话侧解析时即定格(不可变),按到达序
                // 落剪贴板 = last-wins,不存在「在已失效坐标上继续
                // 操作」—— 坐标失效由会话侧世代校验 + resolve/越界
                // 校验把关;过期(Stale)给轻量反馈且不覆盖已有剪贴板,
                // 淘汰(Evicted)同样只反馈不复制;revision 保留作审计。
                // 搜索应答只记日志,不对旧命中做任何 UI 动作。
                if let Some(rx) = self.reply_rx.take() {
                    while let Ok(reply) = rx.try_recv() {
                        match reply {
                            SessionReply::Copy {
                                text,
                                revision,
                                stale,
                            } => {
                                if stale {
                                    // 世代过期:清屏/resize/屏幕切换发生
                                    // 在选区之后 —— 轻量反馈,不覆盖
                                    // 已有剪贴板。
                                    log::warn!(
                                        "复制请求世代过期(会话版本 {}),已拒绝",
                                        revision.0
                                    );
                                    self.status("选区已过期,请重新选择", ctx);
                                } else if let Some(text) = text {
                                    let len = text.len();
                                    self.set_clipboard(text);
                                    log::info!(
                                        "已复制({len} 字节,会话版本 {})",
                                        revision.0
                                    );
                                } else {
                                    // 世代匹配但锚点已滚出历史:明确
                                    // 失效,轻量反馈。
                                    log::info!("复制目标已滚出历史,未复制");
                                    self.status("内容已滚出历史", ctx);
                                }
                            }
                            SessionReply::Selection { text, revision } => {
                                if let Some(text) = text {
                                    let len = text.len();
                                    self.set_clipboard(text);
                                    log::info!("yank {len} 字节(会话版本 {})", revision.0);
                                }
                            }
                            SessionReply::Search { found, revision } => {
                                log::debug!("搜索命中: {found}(会话版本 {})", revision.0);
                            }
                        }
                    }
                    self.reply_rx = Some(rx);
                }
                // 终端事件。
                while let Ok(ev) = self.rx.try_recv() {
                    match ev {
                        // P2b:完全收起时 PTY 输出不触发重绘(内容在下次
                        // 呼出时按当前网格重绘;show 自带 request_redraw)。
                        Event::Wakeup
                            if !self.vis.is_hidden() => {
                                ctx.request_redraw();
                            }
                        Event::Title(t) => {self.tab_title=t;ctx.request_redraw();},
                        Event::ClipboardStore(_, text) => {
                            self.set_clipboard(text);
                        }
                        Event::Exit | Event::ChildExit(_) => {
                            log::info!("标签 shell 已退出");
                            self.close_tab(self.active_tab,ctx);
                            break;
                        }
                        _ => {}
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
                                let _ = sender.send(SessionCommand::Shutdown);
                            }
                            ctx.exit();
                        }
                        crate::ipc::IpcCommand::NewTab => {self.new_tab(ctx);self.show(ctx);},
                        crate::ipc::IpcCommand::NextTab => self.switch_tab((self.active_tab+1)%self.tabs.len(),ctx),
                        crate::ipc::IpcCommand::CloseTab => self.close_tab(self.active_tab,ctx),
                        crate::ipc::IpcCommand::Maximize => {if !self.maximized {self.chrome_action(crate::chrome::Action::Maximize,ctx);}self.show(ctx);},
                        crate::ipc::IpcCommand::Restore => {if self.maximized {self.chrome_action(crate::chrome::Action::Maximize,ctx);}self.show(ctx);},
                        crate::ipc::IpcCommand::Settings => {self.settings_open=true;self.show(ctx);ctx.request_redraw();},
                        crate::ipc::IpcCommand::Pin => {
                            let pinned = self.vis.toggle_pin();
                            log::info!(
                                "固定显示: {}",
                                if pinned {
                                    "开(失焦不再自动隐藏)"
                                } else {
                                    "关"
                                }
                            );
                        }
                    }
                }
                // portal 热键。
                while let Ok(event)=self.hotkey_rx.try_recv(){match event{crate::hotkey::HotkeyTriggered::Toggle=>self.toggle(ctx),crate::hotkey::HotkeyTriggered::Status(s)=>{self.hotkey_status=s;ctx.request_redraw();}}}
                // 配置热重载(§M3:文件 mtime 变化 → 重载颜色等运行时字段)。
                let path = tay_config::Config::config_path();
                let mtime = std::fs::metadata(path).ok().and_then(|m|m.modified().ok());
                if let Some(mtime) = mtime {
                    if self.config_mtime != Some(mtime) && self.config_reload.is_none() {
                        self.config_mtime = Some(mtime);
                        let (tx, rx) = mpsc::channel();
                        self.config_reload = Some(rx);
                        let wake = self.wake.clone();
                        std::thread::spawn(move || {
                            let _ = tx.send(tay_config::Config::try_load());
                            let _ = (&*wake).write(&[0]);
                        });
                    }
                    let loaded = self.config_reload.as_ref().and_then(|rx| match rx.try_recv() {
                        Ok(r) => Some(r),
                        Err(mpsc::TryRecvError::Disconnected) => Some(Err("配置加载线程已退出".into())),
                        Err(mpsc::TryRecvError::Empty) => None,
                    });
                    if let Some(loaded) = loaded {
                        self.config_reload = None;
                        // Discard results for an older file version and schedule the latest.
                        if self.config_mtime != Some(mtime) {
                            let _ = (&*self.wake).write(&[0]);
                            return;
                        }
                            let decision = crate::config::reload_decision(
                                mtime,
                                &mut self.config_error_at,
                                loaded,
                            );
                            match decision {
                            // 失败时保留上次有效配置:半成品文件不能把界面打回默认值。
                                crate::config::Reload::Ok(fresh) => {
                                    // 需重启字段:区分期望(config)与实际运行
                                    // (shell 已启动、output 已绑定、start_hidden 已消费)。
                                    if fresh.global_shortcut != self.config.global_shortcut { self.status("全局快捷键修改需重启并完成桌面授权",ctx); }
                                    if fresh.shell != self.config.shell {
                                        log::warn!(
                                            "shell 变化需重启 tayd 生效(运行中保持 {})",
                                            self.config.shell_command()
                                        );
                                    }
                                    if fresh.output != self.config.output {
                                        log::warn!(
                                            "output 变化需重启 tayd 生效(运行中保持 {})",
                                            if self.config.output.is_empty() {
                                                "合成器默认".to_string()
                                            } else {
                                                self.config.output.clone()
                                            }
                                        );
                                    }
                                    if fresh.start_hidden != self.config.start_hidden {
                                        log::warn!("start_hidden 只在启动时消费,运行中修改无效");
                                    }

                                    let font_changed = fresh.font != self.config.font;
                                    let height_changed =
                                        fresh.height_percent != self.config.height_percent;
                                    let padding_changed = fresh.padding != self.config.padding;
                                    self.config = *fresh;

                                    // 即时生效字段。
                                    if let Some(r) = &mut self.renderer {
                                        r.update_colors(&self.config);
                                        if font_changed { r.update_font(&self.config,ctx.scale_factor()); }
                                        if padding_changed || font_changed {
                                            // pad 变 → 网格列数重算(force)。
                                            r.update_padding(self.config.padding * ctx.scale_factor() as f32);
                                            let (w, h) = r.surface_size();
                                            self.relayout(ctx, w, h, true);
                                        }
                                    }
                                    if height_changed {
                                        // 呼出高度:走 sizing 流程(set_size →
                                        // configure → Resized → 网格重算)。
                                        self.request_layer_height(ctx);
                                    }
                                    log::info!("配置已热重载(即时:字体/颜色/透明度/动画/失焦/padding/高度)");
                                    // 透明度/动画时长/失焦隐藏:渲染与事件处理
                                    // 逐次读 config,替换即生效。
                                    ctx.request_redraw();
                                }
                                crate::config::Reload::Err { repeat, error } => {
                                    if !repeat {
                                        self.status("配置无效，保留当前设置；请检查 tayd.log", ctx);
                                        log::error!(
                                            "配置重载失败,沿用旧配置: {error} \
(期望版本与运行配置分离,见 config.rs)"
                                        );
                                    }
                                }
                            }
                        }
            }

            }
            WEvent::CloseRequested => ctx.exit(),

            // 输出回退/尺寸变化(多显示器):重设高度并重新对齐 margin。
            // 未初始化(首轮 roundtrip 事件)时忽略,走 CanCreateSurfaces 路径。
            // 输出回退:窗口层已把 layer 角色重建到**新 wl_surface**。
            // 丢弃旧渲染器(旧 wl_surface 由窗口层保活),等新 surface
            // 的 configure 握手(Resized)后重建渲染器再呈现。
            WEvent::SurfaceReplaced if self.sender.is_some() => {
                log::info!("输出回退:重建 GPU surface,等待 configure");
                self.renderer = None;
                self.instance = None;
                self.gpu_phase = GpuPhase::AwaitingConfigure { rebuild: true };
            }
            WEvent::SurfaceReplaced => {}

            WEvent::OutputChanged if self.sender.is_some() => {
                self.request_layer_height(ctx);
                // 窗口层重建 layer 角色后 margin 为全离屏,重提当前值。
                let m = self.vis.margin();
                ctx.set_top_margin(m);
                ctx.request_redraw();
            }
            WEvent::OutputChanged => {}

            WEvent::AboutToWait => {
                // 呼出/收起动画步进。
                self.step_animation(ctx);
                // toast 到期:清除并重绘一帧(否则残留到旧画面里)。
                if self
                    .status_msg
                    .as_ref()
                    .is_some_and(|(_, at)| at.elapsed() >= Duration::from_millis(1200))
                {
                    self.status_msg = None;
                    ctx.request_redraw();
                }
                // 截图/基准模式:事件循环自身驱动连续帧。
                let continuous = self.renderer.as_ref().is_some_and(|r| r.shot_pending())
                    || self
                        .bench
                        .is_some_and(|n| (self.bench_times.len() as u32) < n);
                if continuous {
                    ctx.set_poll(true);
                    ctx.request_redraw();
                } else {
                    ctx.set_poll(false);
                }
            }

            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::GpuPhase;

    /// 六、状态转换测试:覆盖「输出回退、配置高度变化和 configure
    /// 交错」的完整事件序列,不只验证单个字段。
    #[test]
    fn gpu_phase_transitions() {
        // 初始 Ready。
        let p = GpuPhase::Ready;
        // 高度请求:Ready → AwaitingConfigure{rebuild:false}。
        let p = p.on_height_request();
        assert_eq!(p, GpuPhase::AwaitingConfigure { rebuild: false });
        // 输出回退(与高度请求交错):rebuild:true。
        let p = GpuPhase::AwaitingConfigure { rebuild: true };
        // 再次高度请求(交错):重建标志必须保留。
        let p = p.on_height_request();
        assert_eq!(
            p,
            GpuPhase::AwaitingConfigure { rebuild: true },
            "高度请求不得覆盖重建需求"
        );
        // configure 握手:重建分支。
        assert!(p.on_resized_handshake());
        // Ready 下误触 Resized:不重建。
        assert!(!GpuPhase::Ready.on_resized_handshake());
    }
}
