# M0 实施笔记(2026-10-16)

M0 落地时发现的、与设计文档假设有出入的 API 事实与工程坑。M1+ 会持续用到。

## alacritty_terminal 0.26:架构已自包含

0.26 相对旧版(0.13 时代)是大重构,**设计文档 §5 的假设需要更新**:

- **PTY 内置**:`alacritty_terminal::tty` 提供 `tty::new(&Options, WindowSize, window_id) -> Pty`,不需要 portable-pty / 手写 forkpty。
- **事件循环内置**:`event_loop::EventLoop::new(Arc<FairMutex<Term<U>>>, U: EventListener, pty, ..)` + `.channel() -> EventLoopSender` + `.spawn()` 起 PTY 线程。
- **键盘翻译由前端负责**:term 不再有 `input()` 方法。键盘 → 字节翻译(`Msg::Input(bytes)`)全部自己做(见 `crates/tayd/src/keys.rs`)。kitty 协议等都要前端实现。
- **渲染取数用 `renderable_content()`**:一次给出 `display_iter`(网格)、`selection`、`cursor: RenderableCursor`、`colors: &Colors`、`display_offset`、`mode`。
- **无 TermSizeInfo**:`Dimensions` trait 需自己实现(其 `(usize,usize)` 实现仅 `#[cfg(test)]` 可见)。
- **调色板默认全空**:`Colors = [Option<Rgb>; 269]`,`Config::default()` 不填任何颜色 —— 前端负责解析 `Color::Named/Indexed`(未覆盖时用标准 ANSI 16 色),OSC 4 可覆盖。
- `Event::PtyWrite(String)`(term→PTY 应答)必须由 Listener 转回 `Msg::Input`。
- 键盘模式位:`term.mode()`(TermMode:APP_CURSOR/APP_KEYPAD/VI…)。

## winit 0.31-beta.3:实测坑

- **Window 是 trait**:`create_window -> Result<Box<dyn Window>, _>`;`with_inner_size` → `with_surface_size`;`CursorMoved/MouseInput` → `PointerMoved/PointerButton`(button 是 `ButtonSource` 枚举)。
- **用户事件机制变了**:无泛型 `EventLoop<T>`;`EventLoopProxy::wake_up()` + `ApplicationHandler::proxy_wake_up()` + 自建 mpsc 通道。
- **表面生命周期**:`resumed()` 之外还有 **`can_create_surfaces()`(必实现)** 才能建窗口/surface。
- **IME 新 API**:`set_ime_allowed/set_ime_cursor_area` 已弃用 → `request_ime_update(ImeRequest::Enable/Update/Disable)`;`ImeRequestData::with_cursor_area(position, size)`(非 Option);Preedit 光标是 UTF-8 字节区间。
- **⚠️ 连续重绘死锁(重要)**:Wayland 后端只有收到合成器 frame callback 才派发 `RedrawRequested`;**`pre_present_notify()` 必须在 `present()` 之前调用**,否则 frame callback 请求丢失 → 第一帧后事件循环永久静默(表现为"渲染了一帧就冻结")。截图/基准等连续帧模式还需配合 `ControlFlow::Poll`。
- winit 用 rwh 0.6,wgpu 30 也用 rwh 0.6.2,兼容;surface 经 `SurfaceTargetUnsafe::RawHandle` 建立。

## wgpu 30:API 变化

- `request_adapter` 返回 `Result`;`RequestAdapterOptions` 新增 `apply_limit_buckets`;`InstanceDescriptor` 用 `new_without_display_handle()`。
- `request_device(desc)` 单参;`DeviceDescriptor` 新增 `experimental_features`。
- `SurfaceConfiguration` 新增 `color_space`(默认 `SurfaceColorSpace::Auto`)。
- **`get_current_texture()` 不再返回 Result**:返回 `CurrentSurfaceTexture::{Success, Suboptimal, Timeout, Occluded, Outdated, Lost, Validation}`。
- **`present` 移到 `Queue::present(SurfaceTexture)`**。
- `PipelineLayoutDescriptor`:无 `push_constant_ranges`,有 `immediate_size`;`bind_group_layouts: &[Option<&BGL>]`;`VertexState.buffers: &[Option<VertexBufferLayout>]`。
- `RenderPassDescriptor` 新增 `multiview_mask`;`RenderPassColorAttachment` 新增 `depth_slice`。
- `device.poll(PollType::wait_indefinitely())`;`get_mapped_range()` 返回 `Result`。
- **sRGB surface 的颜色纪律**:`Rgba8UnormSrgb` 等 sRGB 格式下,硬件对写入自动做 linear→sRGB 编码。清屏色、自绘 quad 颜色都必须先 `srgb_to_linear`;glyphon 文本颜色由其 `ColorMode::Accurate` 自行处理(传 sRGB 值即可)。截图读回得到的是已编码值,直接写 PNG 就是正确显示色。

## 实测数据(本机:KDE Plasma 6 Wayland + AMD GPU)

- 单元格测量:JetBrainsMono Nerd Font 12pt @2x → 19.2×40.0px,网格 56×17 @1100×700。
- 基准:3000 行滚动输出下 300 帧 —— **avg 0.75ms / p50 0.73ms / p99 1.10ms / max 1.39ms ≈ 1339fps**(CPU 提交时间;显示受 vsync 60Hz 限制,M0 验收 150fps 达成)。
- 验证工具:`TAY_SHOT=path[,frames]`(截图退出)、`TAY_BENCH=n`(滚动基准)、`TAY_EXEC=cmd`(启动后执行命令,端到端验证)。

## 遗留(M1 前)

- 单元格对齐:宽字符(CJK)当前依赖 shaping 自然宽度,不做严格 2 倍格对齐——中文下渲染正常,但列对齐场景(如 box-drawing 表格)可能差半格,后续需按 cosmic-term 方式做 cell-aligned 整形。
- 连字已随 shaping 生效(整行 run 内),未专门验收。
- fish 是首要目标 shell,当前验证用 bash(fish 用户环境可自行 `cargo run` 验证)。
