//! 窗口层:SCTK + wlr-layer-shell 手写 Wayland 后端。
//!
//! 对外 API(`Event`/`Handler`/`Ctx`)与 winit 版同形,tayd 无感换轨。
//! 设计要点(DESIGN §6.2 / §4.2):
//! - layer-shell overlay 层,呼出/收起只改 top margin(负值移出屏),
//!   不销毁 surface —— GPU 资源与渲染管线全程常驻,呼出零重建。
//! - 帧回调驱动 RedrawRequested;`pre_present_notify` 必须在 present 前调用。

use std::collections::VecDeque;
use std::os::fd::AsRawFd;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Instant;
use std::time::Duration;

use sctk::compositor::{CompositorHandler, CompositorState};
use sctk::delegate_registry;
use sctk::output::{OutputHandler, OutputState};
use sctk::registry::{ProvidesRegistryState, RegistryState};
use sctk::registry_handlers;
use sctk::shell::WaylandSurface;
use sctk::seat::keyboard::{
    KeyEvent as SctkKeyEvent, KeyboardHandler, Keysym, Modifiers as SctkModifiers, RawModifiers,
};
use sctk::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use sctk::seat::{Capability, SeatHandler, SeatState};
use sctk::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
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
    Preedit { text: String, cursor: Option<(usize, usize)> },
    Commit(String),
}

#[derive(Clone, Debug)]
pub enum Event {
    /// 首次 layer configure 到达,可以建渲染 surface。
    CanCreateSurfaces,
    Resized { width: u32, height: u32 },
    ScaleFactor(f64),
    /// keyboard enter / leave。
    Focused(bool),
    Keyboard { key: KeyPress, pressed: bool, mods: Mods },
    Ime(ImeEvent),
    PointerMoved { x: f64, y: f64 },
    PointerButton { button: MouseButtonIdx, pressed: bool, x: f64, y: f64 },
    Scroll { lines_x: f32, lines_y: f32, pixel_x: f32, pixel_y: f32 },
    CloseRequested,
    RedrawRequested,
    /// 跨线程唤醒(PTY 输出到达)。
    ProxyWake,
    /// 事件循环每轮触发(动画驱动)。
    AboutToWait,
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

impl Ctx<'_> {
    /// 请求下一帧(帧回调到达后投递 `RedrawRequested`)。
    pub fn request_redraw(&mut self) {
        self.state.redraw_requested = true;
        self.state.arm_frame();
    }

    /// Wayland 帧节奏:present 前调用,向合成器请求下一帧回调。
    pub fn pre_present_notify(&mut self) {
        self.state.arm_frame();
    }

    pub fn set_ime_cursor_area(&mut self, x: f32, y: f32, w: f32, h: f32) {
        self.state.ime_rect = Some((x as i32, y as i32, w as i32, h as i32));
        if self.state.ime_active {
            if let Some(ti) = &self.state.text_input {
                ti.set_cursor_rectangle(x as i32, y as i32, w as i32, h as i32);
                ti.commit();
            }
        }
    }

    pub fn surface_size(&self) -> (u32, u32) {
        self.state.size
    }

    pub fn scale_factor(&self) -> f64 {
        self.state.scale
    }

    /// 顶层 margin(px):负数 = 移出屏幕上沿(收起),0 = 呼出。
    /// 呼出/收起动画由 tayd 按帧驱动此值。
    pub fn set_top_margin(&mut self, margin: i32) {
        if let Some(layer) = &self.state.layer {
            layer.set_margin(margin, 0, 0, 0);
            layer.commit();
        }
    }

    /// 设置 layer 表面尺寸(width 0 = 由合成器定,锚定左右时即全宽)。
    pub fn set_size(&mut self, width: u32, height: u32) {
        if let Some(layer) = &self.state.layer {
            layer.set_size(width, height);
            layer.commit();
        }
    }

    /// 当前输出的逻辑尺寸(xdg-output;多显示器时为首个输出)。
    pub fn output_logical_size(&self) -> Option<(u32, u32)> {
        self.state
            .output_state
            .outputs()
            .find_map(|o| self.state.output_state.info(&o))
            .and_then(|i| i.logical_size.map(|(w, h)| (w as u32, h as u32)))
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
    }
}

/// 运行窗口事件循环。layer-shell 无标题栏,`title` 仅日志用。
/// `wake`:跨线程唤醒通道(PTY/IPC/热键线程发 send,事件循环收到后投递 ProxyWake)。
pub fn run(
    title: &str,
    handler: impl Handler,
    wake: (Sender<()>, Receiver<()>),
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
    layer: Option<LayerSurface>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    text_input: Option<ZwpTextInputV3>,
    ime_active: bool,
    ime_rect: Option<(i32, i32, i32, i32)>,
    /// 待投递给 handler 的事件(Dispatch 回调只入队)。
    pending: VecDeque<Event>,
    size: (u32, u32),
    scale: f64,
    configured: bool,
    frame_pending: bool,
    redraw_requested: bool,
    /// arm_frame 时间:帧回调迟迟不来(表面从未呈现过)时兜底派发。
    frame_armed_at: Option<Instant>,
    /// 合成器疑似遮挡/锁屏(帧回调长时间中断):暂停渲染请求。
    stalled: bool,
    poll: bool,
    mods: Mods,
    exit: bool,
    /// 发送端保活:克隆给各线程后本端也必须活着。
    #[allow(dead_code)]
    wake_tx: Sender<()>,
    wake_rx: Receiver<()>,
}

impl WindowState {
    fn new(
        wake: (Sender<()>, Receiver<()>),
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
            layer: None,
            keyboard: None,
            pointer: None,
            text_input: None,
            ime_active: false,
            ime_rect: None,
            pending: VecDeque::new(),
            size: (0, 480),
            scale: 1.0,
            configured: false,
            frame_pending: false,
            redraw_requested: false,
            frame_armed_at: None,
            stalled: false,
            poll: false,
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
        let chosen_output = state
            .output_state
            .outputs()
            .find(|o| {
                state.output_state.info(o).is_some_and(|i| {
                    output_name.map_or(true, |n| i.name.as_deref() == Some(n))
                })
            });
        if let Some(name) = output_name {
            if chosen_output.is_none() {
                log::warn!("配置的输出 {name} 未找到,使用合成器默认");
            }
        }
        // layer 角色(TOP 锚定,初始负 margin 保证不闪屏)。
        let layer = layer_shell.create_layer_surface(
            &qh2,
            state.surface.clone(),
            Layer::Top,
            Some("tay"),
            chosen_output.as_ref(),
        );
        layer.set_anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
        layer.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
        layer.set_exclusive_zone(0);
        layer.set_size(0, 480);
        layer.set_margin(-2000, 0, 0, 0);
        layer.commit();
        state.layer = Some(layer);

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
            self.frame_pending = true;
            self.frame_armed_at = Some(Instant::now());
            self.surface.frame(&self.qh, sctk::compositor::FrameCallbackData(self.surface.clone()));
            self.surface.commit();
        }
    }

    fn main_loop(
        &mut self,
        handler: &mut dyn Handler,
        event_queue: &mut wayland_client::EventQueue<Self>,
    ) -> Result<(), String> {
        while !self.exit {
            // 先处理已到达的 Wayland 事件。
            let dispatched = event_queue
                .dispatch_pending(self)
                .map_err(|e| format!("事件分发失败: {e}"))?;
            self.drain_pending(handler);

            // 遮挡检测:帧回调 2s 中断 = 合成器疑似锁屏/遮挡。
            // 此时 acquire/present 会永久阻塞(Mesa WSI),暂停渲染,
            // 事件循环继续运转(IPC/热键/动画仍可用);回调恢复后自愈。
            if !self.stalled
                && self.frame_pending
                && self
                    .frame_armed_at
                    .is_some_and(|t| t.elapsed() > Duration::from_secs(2))
            {
                log::warn!("合成器 2s 无帧回调(锁屏/遮挡?),暂停渲染");
                self.stalled = true;
            }
            // 帧回调后且请求了重绘 → 投递;回调超时(50ms)兜底:
            // 表面从未呈现过时合成器不会发帧回调,不能死等。
            let stale = self.frame_armed_at.is_some_and(|t| t.elapsed() > Duration::from_millis(50));
            if self.redraw_requested && (!self.frame_pending || stale) && !self.stalled {
                self.redraw_requested = false;
                self.frame_armed_at = None;
                self.emit(handler, Event::RedrawRequested);
            } else if self.stalled {
                // 滞留请求丢弃,恢复后由 PTY 输出/操作重新触发。
                self.redraw_requested = false;
            }

            // 跨线程唤醒。
            while self.wake_rx.try_recv().is_ok() {
                self.emit(handler, Event::ProxyWake);
            }

            // 周期驱动(动画)。
            self.emit(handler, Event::AboutToWait);

            event_queue
                .flush()
                .map_err(|e| format!("flush 失败: {e}"))?;

            // 阻塞等新事件(poll 模式则不等待;空转时小睡防热自旋)。
            let timeout = if self.poll { if dispatched == 0 { 2 } else { 0 } } else { 5 };
            match self.conn.prepare_read() {
                Some(guard) => {
                    let fd = guard.connection_fd();
                    unsafe {
                        let mut pfd = libc::pollfd {
                            fd: fd.as_raw_fd(),
                            events: libc::POLLIN,
                            revents: 0,
                        };
                        libc::poll(&mut pfd, 1, timeout);
                    }
                    guard.read().map_err(|e| format!("wayland read 失败: {e}"))?;
                },
                None => {
                    // 事件已在途,回到循环顶部再分发。
                    std::thread::sleep(Duration::from_millis(1));
                },
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
        _surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        self.scale = new_factor as f64;
        self.pending.push_back(Event::ScaleFactor(new_factor as f64));
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
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
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
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for WindowState {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.pending.push_back(Event::CloseRequested);
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let first = !self.configured;
        self.size = (configure.new_size.0.max(1), configure.new_size.1.max(1));
        self.configured = true;
        if first {
            log::info!("layer configure: {}×{}", self.size.0, self.size.1);
            self.pending.push_back(Event::CanCreateSurfaces);
        }
        self.pending.push_back(Event::Resized { width: self.size.0, height: self.size.1 });
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
            self.pointer = self.seat_state.get_pointer(qh, &seat).ok();
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard {
            if let Some(k) = self.keyboard.take() {
                k.release();
            }
        }
        if capability == Capability::Pointer {
            if let Some(p) = self.pointer.take() {
                p.release();
            }
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
        self.pending.push_back(Event::Keyboard {
            key: map_key(event.keysym, event.utf8, event.raw_code),
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
        self.pending.push_back(Event::Keyboard { key, pressed: true, mods: self.mods });
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: SctkKeyEvent,
    ) {
        self.pending.push_back(Event::Keyboard {
            key: map_key(event.keysym, event.utf8, event.raw_code),
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
    }
}

impl PointerHandler for WindowState {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for ev in events {
            match ev.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Leave { .. } => {},
                PointerEventKind::Motion { .. } => {
                    self.pending.push_back(Event::PointerMoved {
                        x: ev.position.0,
                        y: ev.position.1,
                    });
                },
                PointerEventKind::Press { button, .. } => {
                    self.pending.push_back(Event::PointerButton {
                        button: map_button(button),
                        pressed: true,
                        x: ev.position.0,
                        y: ev.position.1,
                    });
                },
                PointerEventKind::Release { button, .. } => {
                    self.pending.push_back(Event::PointerButton {
                        button: map_button(button),
                        pressed: false,
                        x: ev.position.0,
                        y: ev.position.1,
                    });
                },
                PointerEventKind::Axis { vertical, .. } => {
                    if vertical.discrete != 0 {
                        self.pending.push_back(Event::Scroll {
                            lines_x: 0.0,
                            lines_y: vertical.discrete as f32,
                            pixel_x: 0.0,
                            pixel_y: vertical.absolute as f32,
                        });
                    } else if vertical.absolute != 0.0 {
                        self.pending.push_back(Event::Scroll {
                            lines_x: 0.0,
                            lines_y: 0.0,
                            pixel_x: 0.0,
                            pixel_y: vertical.absolute as f32,
                        });
                    }
                },
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
                    if let Some((x, y, w, h)) = state.ime_rect {
                        ti.set_cursor_rectangle(x, y, w, h);
                    }
                    ti.commit();
                }
                state.pending.push_back(Event::Ime(ImeEvent::Enabled));
            },
            TiEvent::Leave { .. } => {
                state.ime_active = false;
                state.pending.push_back(Event::Ime(ImeEvent::Disabled));
            },
            TiEvent::CommitString { text, .. } => {
                state
                    .pending
                    .push_back(Event::Ime(ImeEvent::Commit(text.unwrap_or_default())));
            },
            TiEvent::PreeditString { text, cursor_begin, cursor_end, .. } => {
                let text = text.unwrap_or_default();
                let cursor = if cursor_begin == cursor_end {
                    None
                } else {
                    Some((cursor_begin as usize, cursor_end as usize))
                };
                state
                    .pending
                    .push_back(Event::Ime(ImeEvent::Preedit { text, cursor }));
                // 协议要求:每次 preedit 后重设光标矩形。
                if let Some(ti) = &state.text_input {
                    if let Some((x, y, w, h)) = state.ime_rect {
                        ti.set_cursor_rectangle(x, y, w, h);
                    }
                    ti.commit();
                }
            },
            TiEvent::Done { .. } => {
                // 协议要求:Done 后必须 commit 一次,否则 KWin 暂停该 text-input。
                if let Some(ti) = &state.text_input {
                    if let Some((x, y, w, h)) = state.ime_rect {
                        ti.set_cursor_rectangle(x, y, w, h);
                    }
                    ti.commit();
                }
            },
            TiEvent::DeleteSurroundingText { .. } => {},
            _ => {},
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
        0xff09 => Some(NamedKey::Tab),
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
            let c = text
                .as_deref()
                .and_then(|t| t.chars().next())
                .or_else(|| {
                    if raw == 0x20 {
                        Some(' ')
                    } else if raw > 0x20 && raw < 0x100 {
                        char::from_u32(raw)
                    } else {
                        None
                    }
                });
            match c {
                Some(c) => KeyKind::Char(c),
                None => {
                    return KeyPress {
                        key: KeyKind::Char('\0'),
                        text: None,
                        repeat: false,
                        raw_code,
                    };
                },
            }
        },
    };
    KeyPress { key, text, repeat: false, raw_code }
}

fn map_button(button: u32) -> MouseButtonIdx {
    match button {
        0x110 => MouseButtonIdx::Left,
        0x111 => MouseButtonIdx::Right,
        0x112 => MouseButtonIdx::Middle,
        n => MouseButtonIdx::Other(n as u16),
    }
}
