# M1 实施笔记(2026-10-16)

M1 = Wayland 独占路线(用户决策:暂不做 X11):tay-window 从 winit 换轨到手写 SCTK + wlr-layer-shell,daemon/IPC/CLI、portal 热键、margin 动画、失焦隐藏。winit/X11 代码保留在 git 历史(bffe663)。

## 架构变化

- **tay-window = SCTK 0.21 手写窗口层**:wayland-client 0.31.15(system 特性 = libwayland 后端)+ smithay-client-toolkit 0.21.1(RegistryState/SeatState/CompositorState/LayerShell/OutputState)+ wayland-protocols-wlr + text-input-v3(wayland-protocols 0.32)。
- **隐藏/呼出模型**:layer surface 全程 mapped,呼出/收起只动 top margin(0 ↔ -layer_h)。GPU surface、swapchain、终端会话零重建 —— 这就是热呼出 16ms/1ms 的原因。**KWin 验证:负 margin 可行**。
- 事件循环:自研 dispatch 循环(dispatch_pending + prepare_read/poll(fd, timeout)),5ms 超时兼作跨线程唤醒节拍(waker = mpsc)。Poll 模式(截图/基准连续帧)空转时小睡 2ms。
- 帧回调驱动 RedrawRequested;**pre_present_notify 在 present 前调用**。

## 五个深坑(全部实测踩过)

1. **Mesa WSI 二次 configure 段错误**:wgpu `surface.configure()` 会销毁重建 vulkan swapchain,重复调用(即使尺寸不变)在 Mesa WSI 的 `vkDestroySwapchain` → `wl_proxy_marshal` 处 SEGV。对策:renderer.resize 尺寸未变早退;初始化时先 `set_size` 等 configure 确认后再建 renderer(全程只 configure 一次)。
2. **首帧鸡生蛋**:表面从未呈现过时,KWin 不发帧回调 → 第一帧渲染永远等不到 RedrawRequested。对策:帧回调 **50ms 超时兜底**——超时即强制派发(自愈,也覆盖合成器异常)。
3. **析构顺序段错误**:wl_surface 必须在 wgpu Surface/Renderer 之后析构(vkDestroySwapchain 会 marshal wl_surface)。对策:tay_window::run 里 `state` 声明在 `handler` 之前(局部变量逆序析构 → handler 先亡)。**这个顺序不能动。**
4. **Poll 热自旋灌爆 socket**:连续帧模式下每轮循环提交 margin → EAGAIN。对策:margin 未变不提交 + 空转小睡。
5. **fork 死锁风险**:PTY `tty::new` 内部 fork;zbus/portal 等多线程先起可能死锁。对策:**IPC accept 线程与 portal 线程延迟到 init(PTY fork 之后)再启动**。

## 实测数据(KDE Plasma 6 Wayland + AMD)

- **热呼出:首次 16ms、常驻后 1ms**(验收 <50ms ✅;测量点 = toggle 指令 → 呼出首帧)
- 滚动基准(3000 行输出):avg 0.89ms/帧 ≈ 1123fps —— 与 M0 的 1339fps 同量级,换轨无实质退化
- layer 高度 45% × 1080 = 486px,网格 99×12

## 遗留(M2 前)

- portal 热键 F12 需用户在 KDE 弹窗确认(首次);失焦隐藏(keyboard leave)待真机交互验证
- text-input-v3 已接线,中文输入待实机验证(fcitx/ibus)
- 多显示器:仅首个输出(layer surface 绑定 output=None);`output` 配置项未做
- GNOME 无 layer-shell → M2 的扩展伴侣方案
- 初始 margin 占位 -2000(WindowState::new 时不知 layer 高度,tayd init 后对齐到 -layer_h)
- 隐藏状态下仍在渲染(PTY 输出触发的重绘不跳过)—— 省电优化项
