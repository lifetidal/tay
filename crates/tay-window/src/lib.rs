//! 窗口层:winit 适配,自有事件类型。
//!
//! tayd 只依赖本 crate 的类型;将来换轨自研 SCTK 时替换本 crate 内部实现即可
//! (DESIGN §5「窗口层」)。

pub use winit::error::EventLoopError;
pub use winit::event_loop::EventLoopProxy;
pub use winit::keyboard::{Key, KeyCode, NamedKey, SmolStr};
pub use winit::raw_window_handle;

use winit::application::ApplicationHandler;
use winit::dpi::PhysicalPosition;
use winit::event::{
    ButtonSource, ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent,
};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{ModifiersState, PhysicalKey};
use winit::raw_window_handle::HasDisplayHandle;
use winit::window::{
    ImeCapabilities, ImeEnableRequest, ImeRequest, ImeRequestData, Window as WinitWindow,
    WindowAttributes, WindowId,
};

/// 修饰键状态(自有类型,不泄漏 winit)。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub meta: bool,
}

impl Mods {
    fn from_winit(m: ModifiersState) -> Self {
        Self {
            ctrl: m.control_key(),
            shift: m.shift_key(),
            alt: m.alt_key(),
            meta: m.meta_key(),
        }
    }
}

/// 一次按键(自有类型)。
#[derive(Clone, Debug)]
pub struct KeyPress {
    /// 物理键位(布局无关,如 `KeyW`)。
    pub physical: KeyCode,
    /// 逻辑键(受布局影响,如 AZERTY 布局下的 `Key::Character("z")`)。
    pub logical: Key,
    /// 本次按键产生的文本;`None` 表示非文本键。
    pub text: Option<String>,
    pub repeat: bool,
}

/// IME 事件。
#[derive(Clone, Debug)]
pub enum ImeEvent {
    Enabled,
    Disabled,
    /// 预编辑文本;`cursor` 为光标在 UTF-8 字节中的起止(无 = 隐藏光标)。
    Preedit { text: String, cursor: Option<(usize, usize)> },
    /// 提交文本(应作为输入写入)。
    Commit(String),
}

/// 自有窗口事件。
#[derive(Clone, Debug)]
pub enum Event {
    /// 首次可以创建渲染 surface(wgpu 在此初始化)。
    CanCreateSurfaces,
    /// 表面尺寸变化(物理像素)。
    Resized { width: u32, height: u32 },
    ScaleFactor(f64),
    Focused(bool),
    Keyboard { key: KeyPress, pressed: bool, mods: Mods },
    Ime(ImeEvent),
    PointerMoved { x: f64, y: f64 },
    PointerButton { button: MouseButtonIdx, pressed: bool, x: f64, y: f64 },
    Scroll { lines_x: f32, lines_y: f32, pixel_x: f32, pixel_y: f32 },
    CloseRequested,
    RedrawRequested,
    /// 后台线程通过 EventLoopProxy 唤醒(PTY 输出到达等)。
    ProxyWake,
    /// 事件循环空闲(每轮循环触发,可在此驱动连续重绘)。
    AboutToWait,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButtonIdx {
    Left,
    Middle,
    Right,
    Other(u16),
}

impl From<ButtonSource> for MouseButtonIdx {
    fn from(b: ButtonSource) -> Self {
        match b {
            ButtonSource::Mouse(m) => match m {
                MouseButton::Left => Self::Left,
                MouseButton::Middle => Self::Middle,
                MouseButton::Right => Self::Right,
                MouseButton::Back => Self::Other(3),
                MouseButton::Forward => Self::Other(4),
                other => Self::Other(other as u16),
            },
            ButtonSource::Unknown(n) => Self::Other(n),
            _ => Self::Other(0xff),
        }
    }
}

/// 窗口操作句柄,传给 [`Handler::on_event`]。
pub struct Ctx<'a> {
    window: &'a dyn WinitWindow,
    event_loop: &'a dyn ActiveEventLoop,
}

impl Ctx<'_> {
    pub fn request_redraw(&self) {
        self.window.request_redraw();
    }

    /// Wayland 帧节奏:告知合成器即将 present(降低输入延迟)。
    pub fn pre_present_notify(&self) {
        self.window.pre_present_notify();
    }

    pub fn set_ime_allowed(&self, allowed: bool) {
        if allowed {
            enable_ime(self.window);
        } else {
            let _ = self.window.request_ime_update(ImeRequest::Disable);
        }
    }

    /// IME 候选框位置(物理像素)。
    pub fn set_ime_cursor_area(&self, x: f32, y: f32, w: f32, h: f32) {
        let data = ImeRequestData::default().with_cursor_area(
            winit::dpi::Position::Physical(PhysicalPosition::new(x as i32, y as i32)),
            winit::dpi::Size::Physical(winit::dpi::PhysicalSize::new(w as u32, h as u32)),
        );
        let _ = self.window.request_ime_update(ImeRequest::Update(data));
    }

    pub fn set_title(&self, title: &str) {
        self.window.set_title(title);
    }

    pub fn surface_size(&self) -> (u32, u32) {
        let s = self.window.surface_size();
        (s.width, s.height)
    }

    pub fn scale_factor(&self) -> f64 {
        self.window.scale_factor()
    }

    /// 底层窗口(wgpu 建 surface 用)。
    pub fn window(&self) -> &dyn WinitWindow {
        self.window
    }

    /// 显示句柄(wgpu 建 surface 用)。
    pub fn display_handle(&self) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        self.event_loop.display_handle()
    }

    /// 跨线程唤醒句柄(PTY 线程通知 UI 线程用)。
    pub fn create_proxy(&self) -> EventLoopProxy {
        self.event_loop.create_proxy()
    }

    /// 事件循环模式:Poll = 不等待事件持续运转(截图/基准用)。
    pub fn set_poll(&self, poll: bool) {
        self.event_loop.set_control_flow(if poll {
            winit::event_loop::ControlFlow::Poll
        } else {
            winit::event_loop::ControlFlow::Wait
        });
    }

    pub fn exit(&self) {
        self.event_loop.exit();
    }
}

/// tayd 实现此 trait 接收事件。
pub trait Handler: 'static {
    fn on_event(&mut self, ctx: &mut Ctx<'_>, event: Event);
}

fn enable_ime(window: &dyn WinitWindow) {
    let data = ImeRequestData::default().with_cursor_area(
        winit::dpi::Position::Physical(PhysicalPosition::new(0, 0)),
        winit::dpi::Size::Physical(winit::dpi::PhysicalSize::new(1, 1)),
    );
    let caps = ImeCapabilities::new().with_cursor_area();
    if let Some(req) = ImeEnableRequest::new(caps, data) {
        let _ = window.request_ime_update(ImeRequest::Enable(req));
    }
}

struct WinitApp<H: Handler> {
    handler: H,
    title: String,
    size: (u32, u32),
    window: Option<Box<dyn WinitWindow>>,
    mods: Mods,
    /// 最近一次指针位置(按钮事件不带位置时用)。
    pointer: (f64, f64),
}

impl<H: Handler> ApplicationHandler for WinitApp<H> {
    fn can_create_surfaces(&mut self, event_loop: &dyn ActiveEventLoop) {
        let attrs = WindowAttributes::default()
            .with_title(&self.title)
            .with_surface_size(winit::dpi::Size::Physical(winit::dpi::PhysicalSize::new(
                self.size.0,
                self.size.1,
            )));
        match event_loop.create_window(attrs) {
            Ok(window) => {
                enable_ime(window.as_ref());
                self.window = Some(window);
                let mut ctx = Ctx { window: self.window.as_deref().unwrap(), event_loop };
                self.handler.on_event(&mut ctx, Event::CanCreateSurfaces);
            }
            Err(e) => {
                log::error!("窗口创建失败: {e}");
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &dyn ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(window) = self.window.as_deref() else { return };
        let mut ctx = Ctx { window, event_loop };
        let ev = match event {
            WindowEvent::CloseRequested => Some(Event::CloseRequested),
            WindowEvent::SurfaceResized(size) => {
                Some(Event::Resized { width: size.width, height: size.height })
            },
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                Some(Event::ScaleFactor(scale_factor))
            },
            WindowEvent::Focused(f) => Some(Event::Focused(f)),
            WindowEvent::ModifiersChanged(m) => {
                self.mods = Mods::from_winit(m.state());
                None
            },
            WindowEvent::KeyboardInput { event, .. } => {
                if event.repeat {
                    None // 重复按键:窗口层不重复投递(终端自己处理长按)
                } else {
                    let physical = match event.physical_key {
                        PhysicalKey::Code(c) => c,
                        _ => KeyCode::Unidentified,
                    };
                    Some(Event::Keyboard {
                        key: KeyPress {
                            physical,
                            logical: event.logical_key,
                            text: event.text.map(|t| t.to_string()),
                            repeat: event.repeat,
                        },
                        pressed: event.state == ElementState::Pressed,
                        mods: self.mods,
                    })
                }
            },
            WindowEvent::Ime(ime) => match ime {
                Ime::Enabled => Some(Event::Ime(ImeEvent::Enabled)),
                Ime::Disabled => Some(Event::Ime(ImeEvent::Disabled)),
                Ime::Preedit(text, cursor) => {
                    Some(Event::Ime(ImeEvent::Preedit { text, cursor }))
                },
                Ime::Commit(text) => Some(Event::Ime(ImeEvent::Commit(text))),
                Ime::DeleteSurrounding { .. } => None, // M0:shell 自行处理
                _ => None,
            },
            WindowEvent::PointerMoved { position, .. } => {
                self.pointer = (position.x, position.y);
                Some(Event::PointerMoved { x: position.x, y: position.y })
            },
            WindowEvent::PointerButton { state, button, position, .. } => {
                let (x, y) = (position.x, position.y);
                Some(Event::PointerButton {
                    button: button.into(),
                    pressed: state == ElementState::Pressed,
                    x,
                    y,
                })
            },
            WindowEvent::MouseWheel { delta, .. } => match delta {
                MouseScrollDelta::LineDelta(x, y) => {
                    Some(Event::Scroll { lines_x: x, lines_y: y, pixel_x: 0.0, pixel_y: 0.0 })
                },
                MouseScrollDelta::PixelDelta(p) => {
                    Some(Event::Scroll { lines_x: 0.0, lines_y: 0.0, pixel_x: p.x as f32, pixel_y: p.y as f32 })
                },
                _ => None,
            },
            WindowEvent::RedrawRequested => Some(Event::RedrawRequested),
            _ => None,
        };
        if let Some(ev) = ev {
            self.handler.on_event(&mut ctx, ev);
        }
    }

    fn about_to_wait(&mut self, event_loop: &dyn ActiveEventLoop) {
        let Some(window) = self.window.as_deref() else { return };
        let mut ctx = Ctx { window, event_loop };
        self.handler.on_event(&mut ctx, Event::AboutToWait);
    }

    fn proxy_wake_up(&mut self, event_loop: &dyn ActiveEventLoop) {
        let Some(window) = self.window.as_deref() else { return };
        let mut ctx = Ctx { window, event_loop };
        self.handler.on_event(&mut ctx, Event::ProxyWake);
    }

    fn destroy_surfaces(&mut self, _event_loop: &dyn ActiveEventLoop) {
        self.window = None;
    }
}

/// 运行窗口事件循环;`size` 为初始物理像素尺寸。
pub fn run<H: Handler + 'static>(
    title: &str,
    size: (u32, u32),
    handler: H,
) -> Result<(), EventLoopError> {
    let event_loop = EventLoop::new()?;
    let app = WinitApp {
        handler,
        title: title.into(),
        size,
        window: None,
        mods: Mods::default(),
        pointer: (0.0, 0.0),
    };
    event_loop.run_app(app)
}
