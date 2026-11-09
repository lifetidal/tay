# 呼出时延优化(1+2)Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把快捷键呼出首帧时延从 p50 209ms 降到 ≤ ~120ms(GpuCore 常驻 + 缓存帧即时上屏)。

**Architecture:** 把 wgpu Instance/Adapter/Device/字体栈/glyph 集抽成常驻 `GpuCore`,remap 只重建 surface 层;收起动画期间把最后一帧拷入常驻缓存纹理,remap 后 configure 一完成同步 blit 上屏,再正常重绘。

**Tech Stack:** Rust / wgpu 30 / glyphon / wayland layer-shell(tay-window)/ 现有 img_pipeline 绘制机制。

**Spec:** docs/superpowers/specs/2026-11-06-summon-latency-design.md

## Global Constraints

- 不修改 unmap 焦点策略与窗口层 retired 保活顺序。
- glyph_buffers 4096 条上限保持不变;cached_frame 纹理 ≤ 1 张。
- TAY_SHOT 模式跳过缓存 blit。
- 验收线:`缓存首帧已提交` p50 ≤ ~120ms(基线 209ms),`内容帧已提交` 单独上报。
- CI(ubuntu-latest)无 GPU:GPU 生命周期测试默认 `#[ignore]`,纯逻辑测试进 CI。
- 提交直接进 main,消息风格「类别: 描述」;打包 pkgrel 51 → 52。

## Review Focus

| # | 输入/条件 | 期望行为 | 钉住的测试(Task) |
|---|---|---|---|
| 1 | 混合 GPU/输出迁移后 rebind | 适配器守卫重建 device,daemon 不退出 | adapter_ok 纯测试(Task 2) |
| 2 | 缓存帧与新 surface 尺寸不同(换输出/改高度) | aspect-fit 居中、不越界、不除零 | cached_fit_rect 数值断言(Task 5) |
| 3 | 无缓存帧或 TAY_SHOT 时 remap | 走正常渲染路径,日志口径正确 | 内容帧日志状态机(Task 5) |
| 4 | animation_ms=0 直达收起(零渲染帧) | 缓存保持旧值或空,blit 跳过不 panic | hiding_anim + skip 判定(Task 4/5) |
| 5 | 动画中反向 toggle | 呼出方向动画不捕获半程帧 | hiding_anim 纯测试(Task 4) |
| 6 | 持续输出时收起 | 稳态渲染 capture=false 零拷贝,仅收起动画帧捕获 | 捕获决策 = hiding_anim(Task 4) |

---

### Task 1: 呼出阶段计时打点(StageTimer + 真机基线拆解)

**Files:**
- Create: `crates/tayd/src/summon_timing.rs`
- Modify: `crates/tayd/src/app.rs`(重建握手路径打点)
- Test: `summon_timing.rs` 内模块测试

**Interfaces:**
- Produces: `pub struct StageTimer`;`StageTimer::new() -> Self`;
  `mark(&mut self, name: &'static str)`;
  `summary(&self) -> String` —— 前缀固定 `"呼出阶段"`,含各段名与毫秒。

- [ ] **Step 1: 写失败测试** —— `summon_timing.rs` 模块测试:
  - `marks_are_monotonic`:三次 mark 后 marks 长度 3,耗时非降;
  - `summary_contains_segments`:summary 含各段名与 `ms`;
  - `empty_summary`:未 mark 时 summary 含「总 0ms」。

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p tayd summon_timing`
Expected: FAIL(模块不存在)

- [ ] **Step 3: 实现 StageTimer** 于 `crates/tayd/src/summon_timing.rs`。

- [ ] **Step 4: 运行确认通过**

Run: `cargo test -p tayd summon_timing`
Expected: PASS

- [ ] **Step 5: app.rs 挂点** —— TayApp 新字段 `summon_timer: Option<StageTimer>`;仅在重建握手(rebuild=true)路径打点:toggle/show 发起处建 timer(SurfaceReplaced 之后)、mark 点:SurfaceReplaced 到达、旧 renderer drop 前/后、configure 握手完成(create_renderer 调用前)、首帧 present 后;首帧日志行追加 `summary()`。

- [ ] **Step 6: 真机验证** —— 隔离实例(XDG_RUNTIME_DIR+XDG_CONFIG_HOME 临时目录,WAYLAND_DISPLAY=/run/user/1000/wayland-1)脚本化 toggle 20 次 + TAY_REBIND 3 次,日志出现 `呼出阶段` 分段毫秒。

- [ ] **Step 7: Commit**

```bash
git add crates/tayd/src/summon_timing.rs crates/tayd/src/app.rs
git commit -m "呼出时延: 阶段计时打点(StageTimer),重建路径分段耗时入日志"
```

---

### Task 2: GpuCore 抽取(device 级资源常驻;零行为变化)

**Files:**
- Create: `crates/tay-render/src/gpu_core.rs`
- Modify: `crates/tay-render/src/lib.rs`(Renderer 瘦身、render 签名)、`crates/tayd/src/app.rs`(create_renderer/SurfaceReplaced/instance 字段)
- Test: `gpu_core.rs` 模块测试(纯)、Create `crates/tay-render/tests/gpu_lifecycle.rs`(`#[ignore]`)

**Interfaces:**
- Consumes: 无(独立结构改造)
- Produces:
  - `pub struct FontKey { family: String, weight: u16, size_px: f32 }`(Eq/Hash)
  - `pub struct FormatCache<T> { current: Option<(wgpu::TextureFormat, T)> }`;
    `pub enum CacheDecision { Reuse, RebuildAndDrop, RebuildFirst }`;
    `FormatCache::<T>::decide(&mut self, format) -> CacheDecision`
  - `pub struct GpuCore`;`GpuCore::new() -> Result<Self, String>`(仅 Instance);
    `GpuCore::ensure_device(&mut self, surface: &wgpu::Surface) -> Result<(), String>`
    —— 无 adapter 时获取;已有 adapter 但 `surface.get_capabilities(&adapter).formats` 为空 → 丢弃 adapter/device/glyph_sets 重建(混合 GPU 守卫);
    `pub fn adapter_ok(formats_empty: bool) -> bool`(纯函数,守卫决策)
  - `Renderer::bind(core: &mut GpuCore, surface: wgpu::Surface<'static>, config: &Config, size: (u32,u32), scale_factor: f64, shot: Option<(&str,u32)>) -> Result<Self, String>`(替代 `Renderer::new`;保留 `cell_size()`/`padding()`/`update_colors()`)
  - `Renderer::render(&mut self, core: &mut GpuCore, texture: wgpu::SurfaceTexture, alpha: f32) -> Result<(), String>`
  - `GpuCore` 持有:instance/adapter/device/queue/`fonts: Option<(FontKey, FontStack)>`/`glyph_sets: FormatCache<GlyphSet>`/wallpaper 缓存/kitty 图像缓存/统一缓冲/quad 缓冲/采样器;`GlyphSet` = glyphon Cache + TextAtlas + TextRenderer + Viewport + bg/img 管线 + srgb 标志;`Renderer` 保留 surface/format/alpha_mode/size/cell/padding/颜色字段/per-frame scratch(current_images/glyph_draws/quads)/shot。
  - 字体策略:`bind` 时 FontKey 变化 → 重建 FontStack 并清空 glyph_sets。

- [ ] **Step 1: 写失败纯测试** —— `gpu_core.rs` 模块测试:
  - `format_cache_reuses_same_format`:同 format 两次 decide → Reuse;
  - `format_cache_evicts_old`:不同 format → RebuildAndDrop 且 current 被替换(用带 Drop 计数的哑 T 断言旧值被丢弃);
  - `format_cache_first_build`:空缓存 → RebuildFirst;
  - `adapter_ok_rejects_empty_formats`:adapter_ok(false)=false,adapter_ok(true)=true;
  - `font_key_equality`:同族同重同尺寸相等。

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p tay-render gpu_core`
Expected: FAIL

- [ ] **Step 3: 实现 FormatCache/FontKey/adapter_ok** 于 `crates/tay-render/src/gpu_core.rs`。

- [ ] **Step 4: 运行确认通过** —— 同上命令,Expected: PASS。

- [ ] **Step 5: 结构搬移** —— GpuCore/GlyphSet 按 Interfaces 字段清单搬移;`Renderer::new` 拆为 `GpuCore::new` + `ensure_device` + `Renderer::bind`;render 签名加 core 参数,device/queue/图集访问改走 core。纯机械,无逻辑变化。

- [ ] **Step 6: app.rs 接线** —— `self.instance: Option<wgpu::Instance>` → `self.gpu_core: Option<GpuCore>`;create_renderer 复用 core.instance 建 surface → ensure_device → bind;SurfaceReplaced 只 drop renderer,保留 gpu_core。

- [ ] **Step 7: 全量编译测试**

Run: `cargo test --locked --workspace && cargo clippy --workspace --all-targets && cargo fmt --check`
Expected: 全 PASS / clippy 干净 / fmt 干净

- [ ] **Step 8: GPU 生命周期测试(#[ignore])** —— `gpu_lifecycle.rs`:`bind_twice_keeps_device_and_renders`:同 GpuCore 上对两个 surface 各 bind 一次,断言 device 指针相同、两次 render 均成功;运行方式 `cargo test -p tay-render --test gpu_lifecycle -- --ignored --nocapture`(真机)。另真机隔离实例 TAY_REBIND ×5 无崩溃、TAY_SHOT 截图与基线一致。

- [ ] **Step 9: Commit**

```bash
git add crates/tay-render/src/gpu_core.rs crates/tay-render/src/lib.rs crates/tayd/src/app.rs crates/tay-render/tests/gpu_lifecycle.rs
git commit -m "呼出时延: GpuCore 抽取 —— Instance/Adapter/Device/字体/glyph 集跨 remap 常驻,渲染器只重建 surface 层(含混合 GPU 适配器守卫); 行为零变化"
```

---

### Task 3: 旧 renderer drop 移出呼出关键路径

**Files:**
- Create: `crates/tayd/src/retired_slot.rs`
- Modify: `crates/tayd/src/app.rs`(SurfaceReplaced → 暂存;AboutToWait 清理点 drop)
- Test: `retired_slot.rs` 模块测试(纯)

**Interfaces:**
- Consumes: Task 2 的 `Renderer` 类型
- Produces: `pub struct RetiredSlot<T> { slot: Option<T> }`;`replace(&mut self, v: T)`(旧值立即 drop);`take_and_drop(&mut self)`;`is_empty(&self) -> bool`;app 字段 `retired_renderer: RetiredSlot<Renderer>`。

- [ ] **Step 1: 写失败测试** —— DropCounter 哑类型:replace 两次 → drop 计数 1;take_and_drop 后 is_empty=true;空时 take_and_drop 无 panic。

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p tayd retired_slot`
Expected: FAIL

- [ ] **Step 3: 实现 RetiredSlot**。

- [ ] **Step 4: 运行确认通过** —— Expected: PASS。

- [ ] **Step 5: app.rs 接线** —— SurfaceReplaced:`self.retired_renderer.replace(self.renderer.take().expect("渲染器存在"))`;与 `self.retiring` 清理同一 AboutToWait 分支开头调 `take_and_drop()`。

- [ ] **Step 6: 验证** —— `cargo test -p tayd`;真机 StageTimer 日志:「drop 旧渲染器」段不再出现在 configure→首帧 区间。

- [ ] **Step 7: Commit**

```bash
git add crates/tayd/src/retired_slot.rs crates/tayd/src/app.rs
git commit -m "呼出时延: 旧渲染器 drop 移出关键路径(RetiredSlot 暂存,AboutToWait 延后释放,retired 保活顺序不变)"
```

---

### Task 4: cached_frame 捕获(收起时;稳态零带宽)

**Files:**
- Modify: `crates/tay-render/src/gpu_core.rs`(CachedFrame/capture_frame)、`crates/tay-render/src/lib.rs`(render 加 capture 参数)、`crates/tayd/src/visibility.rs`(hiding_anim)、`crates/tayd/src/app.rs`(传 capture)
- Test: `visibility.rs` 模块测试新增

**Interfaces:**
- Consumes: Task 2 的 `GpuCore`/`Renderer::render`
- Produces:
  - `pub struct CachedFrame { texture: wgpu::Texture, size: (u32, u32) }`
  - `GpuCore::capture_frame(&mut self, frame_view: &wgpu::TextureView, encoder: &mut wgpu::CommandEncoder, size: (u32,u32))` —— 无缓存或尺寸不同 → 重建纹理(copy_texture_to_texture),≤1 张;内部 info 日志 `缓存帧已捕获 {w}x{h}`
  - `Renderer::render(&mut self, core: &mut GpuCore, texture: wgpu::SurfaceTexture, alpha: f32, capture: bool) -> Result<(), String>`
  - `VisibilityController::hiding_anim(&self) -> bool` —— 动画中且目标为隐藏

- [ ] **Step 1: 写失败测试** —— visibility.rs:show→hide 动画中 `hiding_anim()`=true;hide 完成=false;hide 动画中反向 toggle(呼出方向)=false;show→hide 无动画(animation_ms=0)直达后=false。

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p tayd visibility`
Expected: FAIL(hiding_anim 不存在)

- [ ] **Step 3: 实现 `hiding_anim()`**。

- [ ] **Step 4: 运行确认通过** —— Expected: PASS。

- [ ] **Step 5: 捕获接线** —— render 增加 capture 参数:render pass 后 `core.capture_frame(&frame_view, &mut encoder, (w, h))`(capture=true 时);app.rs render 调用点传 `self.vis.hiding_anim()`。

- [ ] **Step 6: 验证** —— `cargo test --locked --workspace`;真机呼出→收起循环,日志出现 `缓存帧已捕获`;持续输出场景确认稳态渲染无捕获日志(零带宽)。

- [ ] **Step 7: Commit**

```bash
git add crates/tay-render/src/gpu_core.rs crates/tay-render/src/lib.rs crates/tayd/src/visibility.rs crates/tayd/src/app.rs
git commit -m "呼出时延: 收起动画帧捕获 cached_frame(稳态零拷贝,纹理有界 1 张)"
```

---

### Task 5: remap 同步 blit 缓存帧 + 双口径日志

**Files:**
- Modify: `crates/tay-render/src/gpu_core.rs`(cached_fit_rect)、`crates/tay-render/src/lib.rs`(blit_cached)、`crates/tayd/src/app.rs`(Resized 挂点 + 日志语义)
- Test: `gpu_core.rs` 模块测试新增(纯)

**Interfaces:**
- Consumes: Task 4 的 `CachedFrame`/`capture_frame`
- Produces:
  - `pub fn cached_fit_rect(src: (u32,u32), dst: (u32,u32)) -> Option<(f32,f32,f32,f32)>` —— dst 内居中 aspect-fit 的 (x,y,w,h),任一维为 0 → None
  - `Renderer::blit_cached(&mut self, core: &mut GpuCore) -> Result<(), String>` —— acquire_texture → Success/Suboptimal → 复用 RenderImage/img_pipeline 画 cached_frame(fit rect,alpha=1.0,内容已含合成结果)→ submit+present;Outdated/Lost → Err(不阻塞)
  - app 字段 `content_at: Option<Instant>`

- [ ] **Step 1: 写失败测试** —— 数值断言:src=200×100 → dst=400×400 得 (0,100,400,200);src=dst → 满幅 (0,0,dst);dst 更宽 → 左右留边居中;src 任一维 0 → None。

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p tay-render cached_fit_rect`
Expected: FAIL

- [ ] **Step 3: 实现 `cached_fit_rect`**。

- [ ] **Step 4: 运行确认通过** —— Expected: PASS。

- [ ] **Step 5: 实现 `blit_cached`**(按 Interfaces)。

- [ ] **Step 6: app.rs 挂点** —— Resized 重建分支(create_renderer Ok 之后、resize_active 之前):
  - `core.cached_frame.is_some() && self.shot.is_none()` → `ctx.pre_present_notify()` → `blit_cached` → `ctx.arm_frame_after_present()` → 日志 `缓存首帧已提交 {}ms`(show_at.elapsed)+ `self.content_at = Some(Instant::now())`;
  - 否则 `self.content_at = Some(self.show_at.take().unwrap_or_else(Instant::now))`。
  - 现有首帧日志改名为 `内容帧已提交 {}ms`(content_at.elapsed;无 blit 时语义与 209ms 基线一致)。随后原有 set_top_margin/request_redraw 流程不变。

- [ ] **Step 7: 验证** —— `cargo test --locked --workspace`;真机隔离实例:呼出→收起→呼出 N=30,日志统计 `缓存首帧已提交` p50 ≤ 120ms、`内容帧已提交` 单独记录并与 Task 1 基线对比;TAY_SHOT 下确认跳过 blit 且截图正常。

- [ ] **Step 8: Commit**

```bash
git add crates/tay-render/src/gpu_core.rs crates/tay-render/src/lib.rs crates/tayd/src/app.rs
git commit -m "呼出时延: remap 后同步 blit 缓存帧上屏 + 缓存/内容双口径首帧日志"
```

---

### Task 6: 全量验收 + 打包 pkgrel 52

**Files:**
- Modify: `packaging/arch/PKGBUILD`(pkgrel 51 → 52)、spec 状态行、项目记忆

- [ ] **Step 1: 真机压力与统计** —— 隔离实例 TAY_REBIND ×10(无 coredump);toggle 循环 30 次统计双口径,验收线 `缓存首帧已提交` p50 ≤ ~120ms。

- [ ] **Step 2: 烟测** —— `tools/panes-smoke.py`、`tools/focus-smoke.py`、`tools/desktop-services-smoke.py` 全部通过。

- [ ] **Step 3: 全量测试** —— `cargo test --locked --workspace && cargo clippy --workspace --all-targets && cargo fmt --check` 全绿。

- [ ] **Step 4: 视觉比对** —— TAY_SHOT 截图与改造前基线一致(面板/文字/壁纸/光标)。

- [ ] **Step 5: 打包** —— pkgrel 51 → 52;`./tools/package-arch.sh`;包内验证后交用户 `sudo pacman -U` 实测。

- [ ] **Step 6: 文档收尾** —— spec 状态改「已实施」并附最终双口径数据;记忆更新。

- [ ] **Step 7: Commit**

```bash
git add packaging/arch/PKGBUILD docs/superpowers/specs/2026-11-06-summon-latency-design.md
git commit -m "呼出时延: 验收完成(缓存首帧 p50 Xms / 内容帧 Yms); 打包: pkgrel 51 → 52"
```
