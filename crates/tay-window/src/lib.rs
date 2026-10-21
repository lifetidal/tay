//! 窗口层:SCTK + wlr-layer-shell 手写 Wayland 后端。
//!
//! 对外 API(`Event`/`Handler`/`Ctx`)与 winit 版同形,tayd 无感换轨。
//! 设计要点(DESIGN §6.2 / §4.2):
//! - layer-shell overlay 层,呼出/收起只改 top margin(负值移出屏),
//!   不销毁 surface —— GPU 资源与渲染管线全程常驻,呼出零重建。
//! - 帧回调驱动 RedrawRequested;`pre_present_notify` 必须在 present 前调用。

use std::collections::VecDeque;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use sctk::compositor::{CompositorHandler, CompositorState};
use sctk::delegate_registry;
use sctk::output::{OutputHandler, OutputState};
use sctk::registry::{ProvidesRegistryState, RegistryState};
use sctk::registry_handlers;
use sctk::seat::keyboard::{
    KeyEvent as SctkKeyEvent, KeyboardHandler, Keysym, Modifiers as SctkModifiers, RawModifiers,
};
use sctk::seat::pointer::{
    CursorIcon, PointerEvent, PointerEventKind, PointerHandler, ThemeSpec, ThemedPointer,
};
use sctk::seat::{Capability, SeatHandler, SeatState};
use sctk::shell::WaylandSurface;
use sctk::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use sctk::shm::{Shm, ShmHandler};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_surface};
use wayland_client::{Connection, Proxy, QueueHandle};
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_manager_v3::ZwpTextInputManagerV3;
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::{
    ContentHint, ContentPurpose, ZwpTextInputV3,
};

// —— 自有类型(tayd 只依赖这些)——

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub meta: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamedKey {
    Enter,
    Backspace,
    Tab,
    Escape,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Home,
    End,
    PageUp,
    PageDown,
    Delete,
    Insert,
    F(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyKind {
    Char(char),
    Named(NamedKey),
}

#[derive(Clone, Debug)]
pub struct KeyPress {
    pub key: KeyKind,
    pub text: Option<String>,
    pub repeat: bool,
    /// Linux evdev 键码(kitty keyboard protocol 的 key number)。
    pub raw_code: u32,
}

#[derive(Clone, Debug)]
pub enum ImeEvent {
    Enabled,
    Disabled,
    /// 预编辑文本;`cursor` 为 UTF-8 字节中的光标起止。
    Preedit {
        text: String,
        cursor: Option<(usize, usize)>,
    },
    Commit(String),
}

#[derive(Clone, Debug)]
pub enum Event {
    /// 首次 layer configure 到达,可以建渲染 surface。
    CanCreateSurfaces,
    Resized {
        width: u32,
        height: u32,
    },
    ScaleFactor(f64),
    /// keyboard enter / leave。
    Focused(bool),
    /// 修饰键变化独立于普通按键,供指针交互使用。
    ModifiersChanged(Mods),
    Keyboard {
        key: KeyPress,
        pressed: bool,
        mods: Mods,
    },
    Ime(ImeEvent),
    PointerMoved {
        x: f64,
        y: f64,
    },
    PointerButton {
        button: MouseButtonIdx,
        pressed: bool,
        x: f64,
        y: f64,
    },
    Scroll {
        lines_x: f32,
        lines_y: f32,
        pixel_x: f32,
        pixel_y: f32,
    },
    CloseRequested,
    RedrawRequested,
    /// 跨线程唤醒(PTY 输出到达)。
    ProxyWake,
    /// 事件循环每轮触发(动画驱动)。
    AboutToWait,
    /// 绑定输出消失或尺寸/缩放变化:应用应重设 layer 高度并重提 margin。
    OutputChanged,
    /// layer 角色已重建到新的 wl_surface(输出移除回退):应用必须
    /// 重建 wgpu surface 与渲染器,并完成 configure 握手后才可呈现。
    SurfaceReplaced,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButtonIdx {
    Left,
    Middle,
    Right,
    Other(u16),
}

pub trait Handler: 'static {
    fn on_event(&mut self, ctx: &mut Ctx<'_>, event: Event);
}

/// 窗口操作句柄。
pub struct Ctx<'a> {
    state: &'a mut WindowState,
}

fn physical_size(logical: (u32, u32), scale: f64) -> (u32, u32) {
    (
        (logical.0 as f64 * scale).round().max(1.0) as u32,
        (logical.1 as f64 * scale).round().max(1.0) as u32,
    )
}

impl Ctx<'_> {
    /// 请求下一帧(帧回调到达后投递 `RedrawRequested`)。
    pub fn request_redraw(&mut self) {
        self.state.redraw_requested = true;
        self.state.arm_frame();
    }

    /// Wayland 帧节奏:present 前调用(wgpu 语义:通知 swapchain
    /// present 即将发生)。注意此处 arm 的空 commit 在 KWin 上不触发
    /// 帧回调 —— 真正的下一帧请求在 present 之后
    /// (arm_frame_after_present,带 buffer 的 commit 才会被响应)。
    pub fn pre_present_notify(&mut self) {
        self.state.arm_frame();
    }

    /// present 完成后再请求下一帧回调:此时 commit 携带刚 attach 的
    /// buffer,合成器(KWin)会正常响应 done。
    pub fn arm_frame_after_present(&mut self) {
        self.state.rearm_frame();
    }

    /// Use a hand over controls and an I-beam over terminal text.
    pub fn set_pointer_over_control(&mut self, over_control: bool) {
        let icon = if over_control {
            CursorIcon::Pointer
        } else {
            CursorIcon::Text
        };
        if self.state.pointer_icon == Some(icon) {
            return;
        }
        if let Some(pointer) = &self.state.pointer
            && pointer.set_cursor(&self.state.conn, icon).is_ok()
        {
            self.state.pointer_icon = Some(icon);
        }
    }

    pub fn set_ime_cursor_area(&mut self, x: f32, y: f32, w: f32, h: f32) {
        let scale = self.state.scale as f32;
        let rect = (
            (x / scale).round() as i32,
            (y / scale).round() as i32,
            (w / scale).ceil() as i32,
            (h / scale).ceil() as i32,
        );
        if self.state.ime_rect == Some(rect) {
            return; // 未变:不产生新的 commit(避免无意义往返)。
        }
        self.state.ime_rect = Some(rect);
        if self.state.ime_active {
            self.state.apply_ime();
        } else {
            self.state.ime_dirty = true; // 激活时再发。
        }
    }

    pub fn surface_size(&self) -> (u32, u32) {
        physical_size(self.state.size, self.state.scale)
    }

    pub fn scale_factor(&self) -> f64 {
        self.state.scale
    }

    /// 显式唤醒呈现:用户要求呼出/切换时调用 —— 隐藏启动(或完全
    /// 收起)期间合成器不回帧回调,遮挡 watchdog 会在 2s 后误判为
    /// 锁屏/遮挡并暂停渲染(丢弃重绘请求),此后窗口将永远呼不出
    /// (实测复现:隐藏启动 → 2s 后 show,重绘全部被 stalled 丢弃)。
    /// 呼出是用户显式意图:清除 stalled 让下一次 commit 重新呈现;
    /// 若合成器确实无响应,watchdog 会在 2s 后重新暂停(可自愈、
    /// 不复发)。
    pub fn wake_present(&mut self) {
        self.state.stalled = false;
        if self.state.phase != SurfacePhase::Unmapped {
            return;
        }
        // Remap after a null-buffer unmap. Do NOT wait for a fresh configure:
        // KWin sends one configure when the surface unmaps (dropped below while
        // Unmapped) and no second one for a remap whose requested state is
        // unchanged —— waiting here deadlocks every show after the first hide
        // (实测: 隐藏正常、再也呼不出; present 被 AwaitingConfigure 挡住,
        // 日志只有「layer remap: waiting for configure」之后 50ms 兜底)。
        // The surface keeps its last configured size, so present can resume
        // immediately; KWin sends a new configure only if it wants a different
        // size, and configure() accepts it (phase is no longer Unmapped).
        if self.state.surface_configured_once {
            self.state.phase = SurfacePhase::Ready;
        } else {
            // Never configured (hide raced the initial handshake): the first
            // configure is still required before present.
            self.state.phase = SurfacePhase::AwaitingConfigure;
        }
        if let Some(layer) = &self.state.layer {
            layer.set_anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
            layer.set_exclusive_zone(0);
            layer.set_size(self.state.requested_size.0, self.state.requested_size.1);
            layer.set_margin(self.state.top_margin, 0, 0, 0);
            layer.set_keyboard_interactivity(self.state.keyboard_mode);
            layer.commit();
            log::info!("layer remap: presenting at the last configured size");
        }
    }

    /// Moving a mapped surface offscreen does not deactivate it on KWin.
    /// A null buffer removes it from the compositor's window/focus lifecycle.
    /// Keep the wl_surface and renderer alive; PTYs continue running.
    pub fn unmap(&mut self) {
        if matches!(
            self.state.phase,
            SurfacePhase::Unmapped | SurfacePhase::Closing
        ) {
            return;
        }
        self.state.phase = SurfacePhase::Unmapped;
        self.state.keyboard_mode = KeyboardInteractivity::None;
        self.state.frame_pending = false;
        self.state.frame_armed_at = None;
        self.state.redraw_requested = false;
        self.state.mods = Mods::default();
        if let Some(ti) = &self.state.text_input {
            ti.disable();
            ti.commit();
        }
        self.state.ime_active = false;
        self.state.ime_dirty = false;
        if let Some(layer) = &self.state.layer {
            layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        }
        self.state.surface.attach(None, 0, 0);
        self.state.surface.commit();
        log::info!("layer unmapped: keyboard and window focus released");
    }

    /// 顶层 margin(px):负数 = 移出屏幕上沿(收起),0 = 呼出。
    /// 呼出/收起动画由 tayd 按帧驱动此值。
    /// 显式触发输出回退重建(与 output_destroyed 同一路径:
    /// 新 wl_surface + layer 重绑 + SurfaceReplaced/OutputChanged)。
    /// 供恢复流程与 TAY_REBIND 验收钩子使用。
    pub fn force_rebind(&mut self) {
        // rebind_layer 内部已推 SurfaceReplaced + OutputChanged。
        self.state.rebind_layer(None);
        self.state.bound_output = None;
    }

    /// 显式呼出时请求键盘焦点；获得焦点后恢复 OnDemand。
    pub fn request_keyboard(&mut self, shown: bool, acquire: bool) {
        self.state.keyboard_mode = if !shown {
            KeyboardInteractivity::None
        } else if acquire {
            KeyboardInteractivity::Exclusive
        } else {
            KeyboardInteractivity::OnDemand
        };
        if self.state.phase != SurfacePhase::Unmapped
            && let Some(layer) = &self.state.layer
        {
            layer.set_keyboard_interactivity(self.state.keyboard_mode);
            layer.commit();
        }
    }

    pub fn set_top_margin(&mut self, margin: i32) {
        self.state.top_margin = margin;
        if self.state.phase != SurfacePhase::Unmapped
            && let Some(layer) = &self.state.layer
        {
            layer.set_margin(margin, 0, 0, 0);
            layer.commit();
        }
    }

    /// 设置 layer 表面尺寸(width 0 = 由合成器定,锚定左右时即全宽)。
    pub fn set_size(&mut self, width: u32, height: u32) {
        self.state.requested_size = (width, height);
        if self.state.phase != SurfacePhase::Unmapped
            && let Some(layer) = &self.state.layer
        {
            layer.set_size(width, height);
            layer.commit();
        }
    }

    /// 当前输出的逻辑尺寸(xdg-output):优先 layer 绑定的输出,
    /// 其次第一个有信息的输出(多显示器时保证高度算对)。
    pub fn output_logical_size(&self) -> Option<(u32, u32)> {
        let bound = self
            .state
            .bound_output
            .as_ref()
            .and_then(|o| self.state.output_state.info(o))
            .and_then(|i| i.logical_size.map(|(w, h)| (w as u32, h as u32)));
        bound.or_else(|| {
            self.state
                .output_state
                .outputs()
                .find_map(|o| self.state.output_state.info(&o))
                .and_then(|i| i.logical_size.map(|(w, h)| (w as u32, h as u32)))
        })
    }

    pub fn commit(&mut self) {
        if let Some(layer) = &self.state.layer {
            layer.commit();
        }
    }

    /// 事件循环等待模式:Poll = 不等待(pending 事件为空的轮次立即返回)。
    pub fn set_poll(&mut self, poll: bool) {
        self.state.poll = poll;
    }

    /// 动画步进 deadline:事件循环空闲阻塞时最晚在此刻醒来执行下一步。
    /// 动画结束(或无需步进)传 None → 无限阻塞,等真实事件。
    pub fn set_anim_deadline(&mut self, deadline: Option<Instant>) {
        self.state.anim_deadline = deadline;
    }

    /// 一次性唤醒点(如 toast 到期需要重绘一帧):到点后事件循环醒来
    /// 走一轮 AboutToWait,不产生任何事件。仅接受更早的请求。
    pub fn request_wake_at(&mut self, at: Instant) {
        if self.state.wake_at.is_none_or(|cur| at < cur) {
            self.state.wake_at = Some(at);
        }
    }

    /// 底层 Wayland 句柄:(display 指针, surface 指针) —— wgpu 建 surface 用。
    pub fn raw_wayland(&self) -> Option<(usize, usize)> {
        let d = self.state.conn.backend().display_id().as_ptr();
        let s = self.state.surface.id().as_ptr();
        if d.is_null() || s.is_null() {
            return None;
        }
        Some((d as usize, s as usize))
    }

    /// 当前输出名称(如 "DP-1"),M1:呼出所在输出由 tayd 决定。
    pub fn output_name(&self) -> Option<String> {
        self.state
            .output_state
            .outputs()
            .find_map(|o| self.state.output_state.info(&o))
            .and_then(|i| i.name)
    }

    pub fn exit(&mut self) {
        self.state.exit = true;
        self.state.phase = SurfacePhase::Closing;
    }
}

/// 运行窗口事件循环。layer-shell 无标题栏,`title` 仅日志用。
/// `wake`:跨线程唤醒管道(PTY/IPC/热键线程向写端写 1 字节,事件循环
/// poll 它,醒来投递 ProxyWake)—— 管道可进 poll,空闲时才能无限阻塞。
pub fn run(
    title: &str,
    handler: impl Handler,
    wake: (Arc<UnixStream>, UnixStream),
    output_name: Option<&str>,
) -> Result<(), String> {
    log::info!("窗口层:SCTK + wlr-layer-shell({title})");
    // state 必须晚于 handler 析构:wgpu surface/swapchain 销毁时
    // wl_surface 仍需存活(Mesa WSI 会在 vkDestroySwapchain 里 marshal 它)。
    // 局部变量按声明逆序析构:handler 先、state 后 ✓
    let (mut event_queue, mut state) = WindowState::new(wake, output_name)?;
    let mut handler = handler;
    state.main_loop(&mut handler, &mut event_queue)
}

// —— 内部状态 ——

/// surface 世代(架构 §5):每次重建 layer surface 单调 +1。
/// configure/frame/closed 回调按「surface 身份」校验,旧世代事件
/// 不得修改新实例;世代号用于日志与测试断言。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SurfaceGeneration(pub u64);

/// layer surface 生命周期阶段(架构 §5.1)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfacePhase {
    /// 尚未绑定 layer 角色(启动/重建间隙)。
    Uninitialized,
    /// 已绑定,等合成器 configure(握手完成前不可呈现)。
    AwaitingConfigure,
    /// 已 configure,可以呈现。
    Ready,
    /// Hidden with a null buffer; no redraw or commit until explicit remap.
    Unmapped,
    /// 事件循环退出中。
    Closing,
}

/// 帧回调兜底:表面从未呈现过时合成器不会回帧回调,超时强制重试。
const FRAME_STALE: Duration = Duration::from_millis(50);
/// 遮挡 watchdog:帧回调 2s 中断视为锁屏/遮挡,暂停渲染。
const FRAME_STALL: Duration = Duration::from_secs(2);

/// 计算事件循环下一次应醒来的 deadline(纯函数,可控时钟测试)。
/// `frame_armed` = 帧回调武装时间(在途);`redraw` = 是否有滞留
/// 重绘(决定帧定时用 50ms 重试档还是 2s watchdog 档)。
/// 返回最早的有效 deadline;全部无效 → None(无限阻塞)。
/// 帧定时已到期时返回不晚于当前时刻(调用方转为 0 超时,
/// 立即走兜底路径)。
fn next_deadline(
    anim: Option<Instant>,
    wake: Option<Instant>,
    frame_armed: Option<Instant>,
    redraw: bool,
) -> Option<Instant> {
    let mut next: Option<Instant> = None;
    for t in [anim, wake].into_iter().flatten() {
        next = Some(next.map_or(t, |cur| cur.min(t)));
    }
    if let Some(armed) = frame_armed {
        let limit = if redraw { FRAME_STALE } else { FRAME_STALL };
        let t = armed + limit;
        next = Some(next.map_or(t, |cur| cur.min(t)));
    }
    next
}

/// 帧回调是否已过重试上限(纯函数,可控时钟测试)。
fn frame_stale(armed: Option<Instant>, now: Instant) -> bool {
    armed.is_some_and(|t| now.duration_since(t) > FRAME_STALE)
}

/// 帧回调是否已过遮挡 watchdog 上限(纯函数,可控时钟测试)。
fn frame_stalled(armed: Option<Instant>, now: Instant) -> bool {
    armed.is_some_and(|t| now.duration_since(t) > FRAME_STALL)
}

/// 遮挡 watchdog 判定(纯函数,可控时钟测试):帧回调 2s 中断、
/// 未因完全隐藏豁免、未已暂停 → 判定遮挡。
/// 完全隐藏(margin + 高度 ≤ 0)是设计状态:合成器对移出屏幕的
/// 窗口不回帧回调属预期,若据此 stalled,呼出的重绘会被丢弃、
/// 窗口永远呼不出(隐藏启动 2s 后呼出实测复现)。
fn occlusion_stall(
    already_stalled: bool,
    frame_pending: bool,
    fully_hidden: bool,
    armed: Option<Instant>,
    now: Instant,
) -> bool {
    !already_stalled && frame_pending && !fully_hidden && frame_stalled(armed, now)
}

struct WindowState {
    conn: Connection,
    qh: QueueHandle<Self>,
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    /// 句柄保活:wl_compositor 不能提前释放。
    #[allow(dead_code)]
    compositor: CompositorState,
    surface: wl_surface::WlSurface,
    /// 已退役的 wl_surface:GPU 侧销毁旧 swapchain 时仍会 marshal
    /// 旧 surface,必须保活到进程结束(每次输出回退最多多一个)。
    /// 退役的(layer 角色, wl_surface)对。旧 surface 必须**活过
    /// wgpu 渲染器的销毁**:渲染器 drop 时才销毁 syncobj/buffer,
    /// 若 surface 先死,合成器报 "Surface got destroyed already"
    /// 协议错误(TAY_REBIND 验收钩子复现,WAYLAND_DEBUG 定位)。
    /// 上一代在**下一次**重建时成对销毁 —— 届时 app 早已处理完
    /// SurfaceReplaced 并销毁渲染器。
    retired: Vec<(LayerSurface, wl_surface::WlSurface)>,
    layer: Option<LayerSurface>,
    keyboard_mode: KeyboardInteractivity,
    requested_size: (u32, u32),
    /// layer-shell 全局保活(输出回退重建 layer surface 时还要用)。
    layer_shell: LayerShell,
    /// layer surface 绑定的输出(输出销毁 → 回退合成器默认)。
    bound_output: Option<wl_output::WlOutput>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<ThemedPointer>,
    shm: Shm,
    pointer_icon: Option<CursorIcon>,
    text_input: Option<ZwpTextInputV3>,
    ime_active: bool,
    /// IME 光标矩形(最近一次发给合成器的值)。
    ime_rect: Option<(i32, i32, i32, i32)>,
    /// 有未提交的 IME 状态变化(矩形/内容类型等):只在 dirty 时 commit,
    /// 避免对每个 Done 都回 commit 造成 ping-pong 风暴。
    ime_dirty: bool,
    /// 当前 top margin(px,负数 = 移出屏幕上沿):完全隐藏
    /// (margin + 高度 ≤ 0)是**设计状态**,遮挡 watchdog 不得
    /// 据此判定锁屏/遮挡 —— 隐藏启动 + 呼出的核心路径修复。
    top_margin: i32,
    /// 待投递给 handler 的事件(Dispatch 回调只入队)。
    pending: VecDeque<Event>,
    size: (u32, u32),
    scale: f64,
    /// 生命周期阶段(替代 configured 布尔,架构 §5.1)。
    phase: SurfacePhase,
    /// surface 世代:每次重建 layer surface +1。
    generation: SurfaceGeneration,
    /// 全局首个 layer surface 是否已 configure(重建 surface 后
    /// 不再重发 CanCreateSurfaces,终端只建一次)。
    surface_configured_once: bool,
    frame_pending: bool,
    redraw_requested: bool,
    /// arm_frame 时间:帧回调迟迟不来(表面从未呈现过)时兜底派发。
    frame_armed_at: Option<Instant>,
    /// 合成器疑似遮挡/锁屏(帧回调长时间中断):暂停渲染请求。
    stalled: bool,
    poll: bool,
    /// 动画步进 deadline(见 Ctx::set_anim_deadline)。
    anim_deadline: Option<Instant>,
    /// 一次性唤醒点(见 Ctx::request_wake_at;过期后由主循环清除)。
    wake_at: Option<Instant>,
    mods: Mods,
    exit: bool,
    /// 发送端保活:克隆给各线程后本端也必须活着。
    #[allow(dead_code)]
    wake_tx: Arc<UnixStream>,
    wake_rx: UnixStream,
}

impl WindowState {
    fn new(
        wake: (Arc<UnixStream>, UnixStream),
        output_name: Option<&str>,
    ) -> Result<(wayland_client::EventQueue<Self>, Self), String> {
        let conn = Connection::connect_to_env().map_err(|e| format!("连接 Wayland 失败: {e}"))?;
        let (globals, mut event_queue) =
            registry_queue_init(&conn).map_err(|e| format!("注册表初始化失败: {e}"))?;
        let qh = event_queue.handle();

        let compositor =
            CompositorState::bind(&globals, &qh).map_err(|_| "合成器无 wl_compositor")?;
        let layer_shell =
            LayerShell::bind(&globals, &qh).map_err(|_| "合成器不支持 wlr-layer-shell")?;

        // 表面(角色延后到 roundtrip 选好输出后)。
        let surface = compositor.create_surface(&qh);

        let registry_state = RegistryState::new(&globals);
        let seat_state = SeatState::new(&globals, &qh);
        let output_state = OutputState::new(&globals, &qh);
        let qh2 = qh.clone();
        let mut state = Self {
            conn,
            qh,
            registry_state,
            seat_state,
            output_state,
            compositor,
            surface,
            retired: Vec::new(),
            layer: None,
            keyboard_mode: KeyboardInteractivity::None,
            requested_size: (0, 480),
            layer_shell,
            bound_output: None,
            keyboard: None,
            pointer: None,
            shm: Shm::bind(&globals, &qh2).map_err(|e| format!("wl_shm: {e}"))?,
            pointer_icon: None,
            text_input: None,
            ime_active: false,
            ime_rect: None,
            ime_dirty: false,
            top_margin: 0,
            pending: VecDeque::new(),
            size: (0, 480),
            scale: 1.0,
            phase: SurfacePhase::Uninitialized,
            generation: SurfaceGeneration(0),
            surface_configured_once: false,
            frame_pending: false,
            redraw_requested: false,
            frame_armed_at: None,
            stalled: false,
            poll: false,
            anim_deadline: None,
            wake_at: None,
            mods: Mods::default(),
            exit: false,
            wake_tx: wake.0,
            wake_rx: wake.1,
        };
        // 等 registry 事件(seat/输出信息)。
        if let Err(e) = event_queue.roundtrip(&mut state) {
            log::error!("初始 roundtrip 失败: {e}");
        }
        // 选输出:配置名 → 首个有信息的输出 → None(合成器默认)。
        let chosen_output = state.output_state.outputs().find(|o| {
            state
                .output_state
                .info(o)
                .is_some_and(|i| output_name.is_none_or(|n| i.name.as_deref() == Some(n)))
        });
        if let Some(name) = output_name
            && chosen_output.is_none()
        {
            log::warn!("配置的输出 {name} 未找到,使用合成器默认");
        }
        // layer 角色(TOP 锚定,初始负 margin 保证不闪屏)。
        state.phase = SurfacePhase::AwaitingConfigure;
        let layer = state.layer_shell.create_layer_surface(
            &qh2,
            state.surface.clone(),
            Layer::Top,
            Some("tay"),
            chosen_output.as_ref(),
        );
        layer.set_anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
        layer.set_keyboard_interactivity(state.keyboard_mode);
        layer.set_exclusive_zone(0);
        layer.set_size(0, 480);
        layer.set_margin(-2000, 0, 0, 0);
        layer.commit();
        state.layer = Some(layer);
        state.bound_output = chosen_output;

        Ok((event_queue, state))
    }

    fn emit(&mut self, handler: &mut dyn Handler, event: Event) {
        let mut ctx = Ctx { state: self };
        handler.on_event(&mut ctx, event);
    }

    fn drain_pending(&mut self, handler: &mut dyn Handler) {
        while let Some(ev) = self.pending.pop_front() {
            self.emit(handler, ev);
        }
    }

    /// 帧回调未在途时请求之(附一次 commit,保证回调被合成器确认)。
    fn arm_frame(&mut self) {
        if !self.frame_pending {
            self.rearm_frame();
        }
    }

    /// 强制重新请求帧回调(兜底/恢复用,不检查在途状态)。
    fn rearm_frame(&mut self) {
        if self.phase != SurfacePhase::Ready {
            return;
        }
        self.frame_pending = true;
        self.frame_armed_at = Some(Instant::now());
        self.surface.frame(
            &self.qh,
            sctk::compositor::FrameCallbackData(self.surface.clone()),
        );
        self.surface.commit();
    }

    /// 有未提交的 IME 状态变化时发出 commit(无变化则静默 ——
    /// 对每个 Done 回 commit 会让合成器再回 Done,形成 ping-pong 风暴)。
    fn apply_ime(&mut self) {
        if !self.ime_dirty {
            return;
        }
        if let Some(ti) = &self.text_input {
            if let Some((x, y, w, h)) = self.ime_rect {
                ti.set_cursor_rectangle(x, y, w, h);
            }
            ti.commit();
        }
        self.ime_dirty = false;
    }

    fn main_loop(
        &mut self,
        handler: &mut dyn Handler,
        event_queue: &mut wayland_client::EventQueue<Self>,
    ) -> Result<(), String> {
        while !self.exit {
            // 一次性唤醒点已到期:清除(否则下轮计算超时恒为 0,热自旋)。
            if self.wake_at.is_some_and(|t| t <= Instant::now()) {
                self.wake_at = None;
            }
            // 先处理已到达的 Wayland 事件。
            let dispatched = event_queue
                .dispatch_pending(self)
                .map_err(|e| format!("事件分发失败: {e}"))?;
            self.drain_pending(handler);

            // 遮挡检测:帧回调 2s 中断 = 合成器疑似锁屏/遮挡。
            // 此时 acquire/present 会永久阻塞(Mesa WSI),暂停渲染,
            // 事件循环继续运转(IPC/热键/动画仍可用);回调恢复后自愈。
            // **完全隐藏(margin + 高度 ≤ 0)除外**:隐藏是设计状态,
            // 合成器对移出屏幕的窗口不回帧回调是预期 —— 若据此
            // stalled,呼出的重绘会被丢弃、窗口永远呼不出(实测:
            // 隐藏启动 2s 后 show 复现)。呼出路径另经
            // Ctx::wake_present 显式清除 stalled。
            let fully_hidden = self.top_margin + self.size.1 as i32 <= 0;
            if occlusion_stall(
                self.stalled,
                self.frame_pending,
                fully_hidden,
                self.frame_armed_at,
                Instant::now(),
            ) {
                log::warn!("合成器 2s 无帧回调(锁屏/遮挡?),暂停渲染");
                self.stalled = true;
            }
            // 帧回调后且请求了重绘 → 投递;回调超时(50ms)兜底:
            // 表面从未呈现过时合成器不会发帧回调,不能死等。
            let stale = frame_stale(self.frame_armed_at, Instant::now());
            if self.redraw_requested
                && self.phase == SurfacePhase::Ready
                && (!self.frame_pending || stale)
                && !self.stalled
            {
                self.redraw_requested = false;
                if self.frame_pending {
                    // 回调疑似丢失(pre_present 的空 commit 注册不被
                    // 合成器响应,如 KWin):强制重新请求,并**重置兜底
                    // 计时**(而不是清 None)—— 若重发的空 commit 仍不被
                    // 响应,50ms 后 stale 会再次触发,循环以兜底节奏
                    // 继续,不会在 2s 后被 stalled 锁死。
                    self.rearm_frame();
                } else {
                    self.frame_armed_at = None;
                }
                self.emit(handler, Event::RedrawRequested);
            } else if self.stalled {
                // 滞留请求丢弃,恢复后由 PTY 输出/操作重新触发。
                self.redraw_requested = false;
            }

            // 跨线程唤醒(管道排水;多次唤醒合并为一次 ProxyWake)。
            let mut sink = [0u8; 64];
            let mut woke = false;
            while self.wake_rx.read(&mut sink).is_ok() {
                woke = true;
            }
            if woke {
                self.emit(handler, Event::ProxyWake);
            }

            // 周期驱动(动画)。
            self.emit(handler, Event::AboutToWait);

            event_queue
                .flush()
                .map_err(|e| format!("flush 失败: {e}"))?;

            // 阻塞等新事件:空闲时无限阻塞(-1);poll 模式不等待,
            // 空转时小睡防热自旋。其余情况**统一取所有有效 deadline
            // 的最小值**(纯函数 next_deadline,可控时钟测试)——
            // 帧回调兜底(50ms 重试 / 2s watchdog,仅未 stalled 时)、
            // 动画步进、一次性唤醒点(toast)互不覆盖:回调延迟不能
            // 吞掉动画与反馈定时;stalled 后帧定时停用(回调到达即
            // 自愈),但动画/toast 定时继续生效。
            let timeout: i32 = if self.poll {
                if dispatched == 0 { 2 } else { 0 }
            } else {
                let now = Instant::now();
                let frame_armed = if self.frame_pending && !self.stalled {
                    self.frame_armed_at
                } else {
                    None
                };
                let next = next_deadline(
                    self.anim_deadline,
                    self.wake_at,
                    frame_armed,
                    self.redraw_requested,
                );
                match next {
                    Some(t) if t > now => {
                        let ms = (t - now).as_millis();
                        // 动画步进粒度上限 ~16ms:动画进行中让循环
                        // 按步进节奏提前醒来;无动画用完整剩余时长。
                        let cap = if self.anim_deadline.is_some() {
                            16
                        } else {
                            5000
                        };
                        ms.min(cap) as i32
                    }
                    Some(_) => 0,
                    None => -1,
                }
            };
            match self.conn.prepare_read() {
                Some(guard) => {
                    let fd = guard.connection_fd();
                    unsafe {
                        let mut pfds = [
                            libc::pollfd {
                                fd: fd.as_raw_fd(),
                                events: libc::POLLIN,
                                revents: 0,
                            },
                            libc::pollfd {
                                fd: self.wake_rx.as_raw_fd(),
                                events: libc::POLLIN,
                                revents: 0,
                            },
                        ];
                        libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, timeout);
                    }
                    // 无数据时(超时/唤醒管道)read 返回 WouldBlock,内部消化。
                    guard
                        .read()
                        .map_err(|e| format!("wayland read 失败: {e}"))?;
                }
                None => {
                    // 事件已在途,回到循环顶部再分发。
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
        Ok(())
    }
}

// —— sctk / wayland Dispatch 实现 ——

delegate_registry!(WindowState);

impl ProvidesRegistryState for WindowState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

impl CompositorHandler for WindowState {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        if surface != &self.surface {
            return;
        }
        self.scale = new_factor as f64;
        self.surface.set_buffer_scale(new_factor.max(1));
        self.pending
            .push_back(Event::ScaleFactor(new_factor as f64));
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        // 旧世代 surface 的帧回调不得清除新实例的在途状态(架构 §5.4)。
        if surface != &self.surface {
            return;
        }
        self.frame_pending = false;
        self.frame_armed_at = None;
        if self.stalled {
            log::info!("合成器帧回调恢复,渲染重启");
            self.stalled = false;
        }
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for WindowState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn update_output(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        // 绑定输出的尺寸/缩放可能变化:通知应用重设 layer 高度
        // (重算是幂等的,应用侧跳过无变化)。
        if self.bound_output.as_ref() == Some(&output) {
            self.pending.push_back(Event::OutputChanged);
        }
    }

    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        // 指定输出消失的明确回退:重建 layer surface 到合成器默认输出,
        // 并通知应用重设高度与 margin(原 surface 由 wgpu 继续使用,
        // 只有 layer 角色重建 —— GPU 资源零重建)。
        if self.bound_output.as_ref() == Some(&output) {
            log::warn!("呼出所在输出已移除,回退到合成器默认输出");
            self.rebind_layer(None);
            self.bound_output = None;
            self.pending.push_back(Event::OutputChanged);
        }
    }
}

impl WindowState {
    /// 重建 layer surface 角色绑定到 `output`(None = 合成器默认)。
    /// 复用同一 wl_surface:wgpu/渲染管线不受影响;margin 重置为
    /// 全离屏(-2000),由应用收到 OutputChanged 后重新对齐。
    fn rebind_layer(&mut self, output: Option<&wl_output::WlOutput>) {
        // 协议要求:layer 角色必须建在**没有已提交 buffer 的**新
        // wl_surface 上(已呈现过的 surface 复建角色是协议错误)。
        // 旧 surface 退役保活(GPU 侧销毁 swapchain 时仍会 marshal 它)。
        // 上一代退役资源此刻才销毁:app 在上一次重建的
        // SurfaceReplaced 里已销毁渲染器,旧 surface 上不再有
        // wgpu 引用(sctk 保证先销毁角色再销毁 surface 的顺序)。
        self.retired.clear();
        // 当前这一代退役**保活**:app 要等 SurfaceReplaced 事件
        // 才 drop 渲染器,surface 必须先活过 wgpu 的销毁。
        if let Some(old_layer) = self.layer.take() {
            let old_surface =
                std::mem::replace(&mut self.surface, self.compositor.create_surface(&self.qh));
            self.retired.push((old_layer, old_surface));
        } else {
            self.surface = self.compositor.create_surface(&self.qh);
        }
        // 世代 +1:旧 surface 的 configure/frame/closed 回调按身份
        // 忽略(架构 §5.4)。awaiting configure 完成前不可呈现。
        self.generation.0 += 1;
        self.phase = SurfacePhase::AwaitingConfigure;
        // 新 surface 的配置状态重置:等首个 configure(握手)后应用
        // 才能重建渲染器并呈现。stalled(遮挡 watchdog)是旧 surface
        // 的状态,必须清除 —— 否则新 surface 没有首帧呈现,恢复
        // 路径永远无法启动。
        self.frame_pending = false;
        self.frame_armed_at = None;
        self.stalled = false;
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            self.surface.clone(),
            Layer::Top,
            Some("tay"),
            output,
        );
        layer.set_anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
        layer.set_keyboard_interactivity(self.keyboard_mode);
        layer.set_exclusive_zone(0);
        self.surface
            .set_buffer_scale(self.scale.round().max(1.0) as i32);
        layer.set_size(0, self.size.1.max(1));
        layer.set_margin(-2000, 0, 0, 0);
        layer.commit();
        self.layer = Some(layer);
        // 应用侧:重建 GPU surface(新的 raw 句柄)并等 configure。
        self.pending.push_back(Event::SurfaceReplaced);
        self.pending.push_back(Event::OutputChanged);
    }
}

impl LayerShellHandler for WindowState {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        // 旧世代 surface 的 closed 不关新实例(架构 §5.4)。
        if layer.wl_surface() != &self.surface {
            return;
        }
        self.pending.push_back(Event::CloseRequested);
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        // 旧世代 surface 的 configure 不得修改新实例(架构 §5.4)。
        if self.phase == SurfacePhase::Unmapped {
            // null-buffer unmap 后 KWin 可能回一次 configure(常为 0×0):
            // 此刻丢弃,remap 由 wake_present 按上次尺寸直接呈现。
            log::debug!(
                "忽略 unmapped 状态的 configure: {}×{}",
                configure.new_size.0,
                configure.new_size.1
            );
            return;
        }
        if layer.wl_surface() != &self.surface {
            log::debug!("忽略旧 surface(世代 {})的 configure", self.generation.0 - 1);
            return;
        }
        let first = !self.surface_configured_once;
        self.size = (configure.new_size.0.max(1), configure.new_size.1.max(1));
        self.phase = SurfacePhase::Ready;
        self.surface_configured_once = true;
        if first {
            log::info!("layer configure: {}×{}", self.size.0, self.size.1);
            self.pending.push_back(Event::CanCreateSurfaces);
        }
        self.pending.push_back(Event::Resized {
            width: self.size.0,
            height: self.size.1,
        });
    }
}

impl SeatHandler for WindowState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self.seat_state.get_keyboard(qh, &seat, None).ok();
            if self.text_input.is_none() {
                self.setup_text_input(&seat);
            }
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            let cursor_surface = self.compositor.create_surface(qh);
            self.pointer = self
                .seat_state
                .get_pointer_with_theme::<_, ()>(
                    qh,
                    &seat,
                    self.shm.wl_shm(),
                    cursor_surface,
                    ThemeSpec::default(),
                )
                .ok();
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard
            && let Some(k) = self.keyboard.take()
        {
            k.release();
        }
        if capability == Capability::Pointer
            && let Some(p) = self.pointer.take()
        {
            p.pointer().release();
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl WindowState {
    /// 绑定 text-input-v3(KWin 支持;缺失则静默降级,IME 不可用但终端可用)。
    fn setup_text_input(&mut self, seat: &wl_seat::WlSeat) {
        let Ok(manager) =
            self.registry_state
                .bind_one::<ZwpTextInputManagerV3, _, _>(&self.qh, 1..=1, ())
        else {
            log::warn!("合成器无 text-input-v3,IME 不可用");
            return;
        };
        let ti = manager.get_text_input(seat, &self.qh, ());
        self.text_input = Some(ti);
    }
}

impl KeyboardHandler for WindowState {
    fn enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _serial: u32,
        _raw: &[u32],
        _keysyms: &[Keysym],
    ) {
        if &self.surface == surface {
            self.pending.push_back(Event::Focused(true));
        }
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        if &self.surface == surface {
            self.pending.push_back(Event::Focused(false));
        }
    }

    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: SctkKeyEvent,
    ) {
        let key = map_key(event.keysym, event.utf8, event.raw_code);
        if key.key == KeyKind::Char('\0') && key.text.is_none() {
            return;
        }
        self.pending.push_back(Event::Keyboard {
            key,
            pressed: true,
            mods: self.mods,
        });
    }

    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: SctkKeyEvent,
    ) {
        let mut key = map_key(event.keysym, event.utf8, event.raw_code);
        key.repeat = true;
        if key.key == KeyKind::Char('\0') && key.text.is_none() {
            return;
        }
        self.pending.push_back(Event::Keyboard {
            key,
            pressed: true,
            mods: self.mods,
        });
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: SctkKeyEvent,
    ) {
        let key = map_key(event.keysym, event.utf8, event.raw_code);
        if key.key == KeyKind::Char('\0') && key.text.is_none() {
            return;
        }
        self.pending.push_back(Event::Keyboard {
            key,
            pressed: false,
            mods: self.mods,
        });
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        modifiers: SctkModifiers,
        _raw: RawModifiers,
        _layout: u32,
    ) {
        self.mods = Mods {
            ctrl: modifiers.ctrl,
            shift: modifiers.shift,
            alt: modifiers.alt,
            meta: modifiers.logo,
        };
        self.pending.push_back(Event::ModifiersChanged(self.mods));
    }
}

impl ShmHandler for WindowState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl PointerHandler for WindowState {
    fn pointer_frame(
        &mut self,
        conn: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for ev in events {
            match ev.kind {
                PointerEventKind::Enter { .. } => {
                    self.pointer_icon = None;
                    if let Some(pointer) = &self.pointer {
                        let _ = pointer.set_cursor(conn, CursorIcon::Default);
                    }
                    self.pending.push_back(Event::PointerMoved {
                        x: ev.position.0 * self.scale,
                        y: ev.position.1 * self.scale,
                    });
                }
                PointerEventKind::Leave { .. } => {
                    self.pointer_icon = None;
                }
                PointerEventKind::Motion { .. } => {
                    self.pending.push_back(Event::PointerMoved {
                        x: ev.position.0 * self.scale,
                        y: ev.position.1 * self.scale,
                    });
                }
                PointerEventKind::Press { button, .. } => {
                    self.pending.push_back(Event::PointerButton {
                        button: map_button(button),
                        pressed: true,
                        x: ev.position.0 * self.scale,
                        y: ev.position.1 * self.scale,
                    });
                }
                PointerEventKind::Release { button, .. } => {
                    self.pending.push_back(Event::PointerButton {
                        button: map_button(button),
                        pressed: false,
                        x: ev.position.0 * self.scale,
                        y: ev.position.1 * self.scale,
                    });
                }
                PointerEventKind::Axis { vertical, .. } => {
                    if vertical.discrete != 0 {
                        self.pending.push_back(Event::Scroll {
                            lines_x: 0.0,
                            lines_y: vertical.discrete as f32,
                            pixel_x: 0.0,
                            pixel_y: vertical.absolute as f32 * self.scale as f32,
                        });
                    } else if vertical.absolute != 0.0 {
                        self.pending.push_back(Event::Scroll {
                            lines_x: 0.0,
                            lines_y: 0.0,
                            pixel_x: 0.0,
                            pixel_y: vertical.absolute as f32 * self.scale as f32,
                        });
                    }
                }
            }
        }
    }
}

impl sctk::dispatch2::Dispatch2<ZwpTextInputManagerV3, WindowState> for () {
    fn event(
        &self,
        _state: &mut WindowState,
        _proxy: &ZwpTextInputManagerV3,
        _event: <ZwpTextInputManagerV3 as Proxy>::Event,
        _conn: &Connection,
        _qhandle: &QueueHandle<WindowState>,
    ) {
    }
}

impl sctk::dispatch2::Dispatch2<ZwpTextInputV3, WindowState> for () {
    fn event(
        &self,
        state: &mut WindowState,
        _proxy: &ZwpTextInputV3,
        event: <ZwpTextInputV3 as Proxy>::Event,
        _conn: &Connection,
        _qhandle: &QueueHandle<WindowState>,
    ) {
        use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::Event as TiEvent;
        match event {
            TiEvent::Enter { .. } => {
                state.ime_active = true;
                if let Some(ti) = &state.text_input {
                    ti.set_content_type(ContentHint::None, ContentPurpose::Terminal);
                    ti.enable();
                }
                // 矩形可能已在激活前由应用设置过:一并提交。
                state.ime_dirty = true;
                state.apply_ime();
                state.pending.push_back(Event::Ime(ImeEvent::Enabled));
            }
            TiEvent::Leave { .. } => {
                state.ime_active = false;
                state.pending.push_back(Event::Ime(ImeEvent::Disabled));
            }
            TiEvent::CommitString { text, .. } => {
                state
                    .pending
                    .push_back(Event::Ime(ImeEvent::Commit(text.unwrap_or_default())));
            }
            TiEvent::PreeditString {
                text,
                cursor_begin,
                cursor_end,
                ..
            } => {
                let text = text.unwrap_or_default();
                let cursor = if cursor_begin == cursor_end {
                    None
                } else {
                    Some((cursor_begin as usize, cursor_end as usize))
                };
                state
                    .pending
                    .push_back(Event::Ime(ImeEvent::Preedit { text, cursor }));
                // preedit 改变光标位置:重设矩形并按协议 commit。
                state.ime_dirty = true;
                state.apply_ime();
            }
            TiEvent::Done { .. } => {
                // 仅提交未应用的状态变化;无变化不回复 ——
                // 无条件 commit 会与合成器形成 commit→done 往返风暴
                // (KWin 每条 commit 都回 done)。
                state.apply_ime();
            }
            TiEvent::DeleteSurroundingText { .. } => {}
            _ => {}
        }
    }
}

sctk::delegate_dispatch2!(WindowState);

// —— 键位映射 ——

fn map_key(keysym: Keysym, text: Option<String>, raw_code: u32) -> KeyPress {
    let raw = keysym.raw();
    let named = match raw {
        0xff0d => Some(NamedKey::Enter),
        0xff08 => Some(NamedKey::Backspace),
        0xff09 | 0xfe20 => Some(NamedKey::Tab),
        0xff1b => Some(NamedKey::Escape),
        0xff51 => Some(NamedKey::ArrowLeft),
        0xff52 => Some(NamedKey::ArrowUp),
        0xff53 => Some(NamedKey::ArrowRight),
        0xff54 => Some(NamedKey::ArrowDown),
        0xff50 => Some(NamedKey::Home),
        0xff57 => Some(NamedKey::End),
        0xff55 => Some(NamedKey::PageUp),
        0xff56 => Some(NamedKey::PageDown),
        0xffff => Some(NamedKey::Delete),
        0xff63 => Some(NamedKey::Insert),
        0xffbe..=0xffc9 => Some(NamedKey::F((raw - 0xffbe + 1) as u8)),
        _ => None,
    };
    let key = match named {
        Some(n) => KeyKind::Named(n),
        None => {
            // UTF-8 text may contain ETX for Ctrl+C. Shortcut identity must
            // come from the keysym; keep the original text for PTY input.
            let c = if (0x20..0x100).contains(&raw) {
                char::from_u32(raw)
            } else {
                text.as_deref()
                    .and_then(|t| t.chars().next())
                    .filter(|c| !c.is_control())
            };
            match c {
                Some(c) => KeyKind::Char(c),
                None => {
                    return KeyPress {
                        key: KeyKind::Char('\0'),
                        text: None,
                        repeat: false,
                        raw_code,
                    };
                }
            }
        }
    };
    KeyPress {
        key,
        text,
        repeat: false,
        raw_code,
    }
}

fn map_button(button: u32) -> MouseButtonIdx {
    match button {
        0x110 => MouseButtonIdx::Left,
        0x111 => MouseButtonIdx::Right,
        0x112 => MouseButtonIdx::Middle,
        n => MouseButtonIdx::Other(n as u16),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 受控时钟:纯 deadline 函数以显式 `now` 为参,用同一基准
    /// Instant 加偏移构造场景(不依赖真实流逝时间)。
    #[test]
    fn buffer_size_uses_output_pixels_not_logical_points() {
        assert_eq!(physical_size((1920, 486), 1.0), (1920, 486));
        assert_eq!(physical_size((1920, 486), 2.0), (3840, 972));
    }

    #[test]
    fn shortcut_identity_survives_control_text() {
        for (raw, text, expected) in [(0x63, "\x03", 'c'), (0x54, "\x14", 'T'), (0x2b, "+", '+')] {
            let key = map_key(Keysym::new(raw), Some(text.into()), 0);
            assert_eq!(key.key, KeyKind::Char(expected));
            assert_eq!(key.text.as_deref(), Some(text));
        }
        assert_eq!(
            map_key(Keysym::new(0xfe20), None, 0).key,
            KeyKind::Named(NamedKey::Tab)
        );
    }

    #[test]
    fn real_xkb_shortcuts_keep_identity_with_ctrl_shift_and_caps_lock() {
        use xkbcommon::xkb;
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "evdev",
            "pc105",
            "us",
            "",
            Some(String::new()),
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap();
        // evdev codes + 8, exactly as used by the Wayland/XKB keyboard stack.
        for (code, shift, expected) in [
            (20, false, KeyKind::Char('t')),
            (20, true, KeyKind::Char('T')),
            (17, false, KeyKind::Char('w')),
            (46, true, KeyKind::Char('C')),
            (47, true, KeyKind::Char('V')),
            (13, false, KeyKind::Char('=')),
            (13, true, KeyKind::Char('+')),
            (12, false, KeyKind::Char('-')),
            (11, false, KeyKind::Char('0')),
            (51, false, KeyKind::Char(',')),
            (41, false, KeyKind::Char('`')),
            (15, false, KeyKind::Named(NamedKey::Tab)),
            (15, true, KeyKind::Named(NamedKey::Tab)),
            (109, false, KeyKind::Named(NamedKey::PageDown)),
            (104, false, KeyKind::Named(NamedKey::PageUp)),
        ] {
            let mut state = xkb::State::new(&keymap);
            state.update_key((29u32 + 8).into(), xkb::KeyDirection::Down);
            if shift {
                state.update_key((42u32 + 8).into(), xkb::KeyDirection::Down);
            }
            let code = (code + 8u32).into();
            let text = state.key_get_utf8(code);
            assert_eq!(
                map_key(state.key_get_one_sym(code), Some(text), 0).key,
                expected
            );
        }
        let mut state = xkb::State::new(&keymap);
        state.update_key(66u32.into(), xkb::KeyDirection::Down); // Caps Lock
        state.update_key(66u32.into(), xkb::KeyDirection::Up);
        state.update_key(37u32.into(), xkb::KeyDirection::Down); // Ctrl
        assert_eq!(
            map_key(
                state.key_get_one_sym(28u32.into()),
                Some(state.key_get_utf8(28u32.into())),
                20
            )
            .key,
            KeyKind::Char('T')
        );
    }

    #[test]
    fn next_deadline_picks_earliest_valid() {
        let base = Instant::now();
        // 全空 → 无限阻塞。
        assert_eq!(next_deadline(None, None, None, false), None);
        // 动画 vs toast:取最早。
        let anim = Some(base + Duration::from_millis(30));
        let wake = Some(base + Duration::from_millis(10));
        assert_eq!(next_deadline(anim, wake, None, false), wake);
        // 帧定时(重绘档 = 50ms 重试)。
        let armed = Some(base);
        assert_eq!(
            next_deadline(None, None, armed, true),
            Some(base + FRAME_STALE)
        );
        // 帧定时(watchdog 档 = 2s)。
        assert_eq!(
            next_deadline(None, None, armed, false),
            Some(base + FRAME_STALL)
        );
        // 帧定时被更早的动画覆盖(动画不因帧兜底被吞)。
        assert_eq!(
            next_deadline(anim, None, armed, false),
            anim,
            "动画 deadline 早于 watchdog,必须优先"
        );
    }

    #[test]
    fn stale_and_stalled_thresholds() {
        let base = Instant::now();
        let armed = Some(base);
        // 未到阈值。
        assert!(!frame_stale(armed, base + Duration::from_millis(49)));
        assert!(!frame_stalled(armed, base + Duration::from_secs(2)));
        // 越过阈值(严格大于)。
        assert!(frame_stale(armed, base + Duration::from_millis(51)));
        assert!(frame_stalled(
            armed,
            base + Duration::from_secs(2) + Duration::from_millis(1)
        ));
        // 未武装 → 永不 stale/stalled。
        assert!(!frame_stale(None, base + Duration::from_secs(60)));
        assert!(!frame_stalled(None, base + Duration::from_secs(60)));
    }

    /// 完全隐藏时帧回调中断不是遮挡(隐藏是设计状态):隐藏启动
    /// 2s 后不得 stalled —— 否则呼出的重绘被丢弃、窗口永远呼
    /// 不出(实测复现的回归)。
    #[test]
    fn hidden_window_never_classifies_as_occluded() {
        let base = Instant::now();
        let armed = Some(base);
        let late = base + Duration::from_secs(3);
        // 可见:帧回调超时 → 判定遮挡。
        assert!(occlusion_stall(false, true, false, armed, late));
        // 完全隐藏:超时也不判定。
        assert!(!occlusion_stall(false, true, true, armed, late));
        // 已 stalled 不重复判定;帧未在途不判定。
        assert!(!occlusion_stall(true, true, false, armed, late));
        assert!(!occlusion_stall(false, false, false, armed, late));
    }

    #[test]
    fn phase_transitions_are_explicit() {
        // 阶段是显式枚举:重建必须回到 AwaitingConfigure(布尔组合
        // 无法表达「configured 但等待新握手」的中间态)。
        assert_ne!(SurfacePhase::Ready, SurfacePhase::AwaitingConfigure);
        assert_ne!(SurfacePhase::Ready, SurfacePhase::Uninitialized);
        assert_ne!(SurfacePhase::Ready, SurfacePhase::Closing);
        // 世代号可比较:重建单调递增。
        assert!(SurfaceGeneration(1) > SurfaceGeneration(0));
    }
}
