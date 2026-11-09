# Tay 呼出时延优化:GPU 资源跨 remap 复用 + 缓存帧即时上屏(设计)

日期:2026-11-06
状态:**已实施**(2026-11-09;方案 1+2 全部落地,52 包已打)

## 最终结果(隔离实例,31 次 toggle / 15 呼出周期)

| 口径 | 基线 | 最终 | 验收线 |
|---|---|---|---|
| 缓存首帧已提交 p50 | —(无此路径) | **6ms**(min 5/max 7) | ≤ ~120ms ✅ |
| 内容帧已提交 p50 | 209ms | **50ms**(min 50/max 97) | 上报观察 ✅ |

- GpuCore 复用:remap 不再新建 Instance/Adapter/Device;字体栈每 daemon 仅建 1 次。
- 旧渲染器 drop 移出关键路径(RetiredSlot,on_event 空闲点释放)。
- 缓存帧:收起动画帧捕获(稳态零带宽),remap 后 configure 一完成同步 blit 上屏。
- TAY_REBIND ×10 零崩溃;三套烟测(panes/focus/desktop-services)全绿;
  271 测试、clippy 0、fmt 干净。
- 实施中发现并修复 3 个真机 bug:跨 Instance 建 surface/device(SurfaceId panic)、
  混合 GPU 重建未同步重建缓冲(跨 device 校验 panic)、连续 SurfaceReplaced
  非幂等取走渲染器(portal 缓冲投递复现,desktop-services-smoke 捕获)。

## 意图与为什么

实测每次快捷键呼出的首帧提交时延(运行中 daemon 日志,378 次样本):
p50 **209ms**、p90 242ms、p99 389ms、最坏 2066ms;且不含 KWin 快捷键 →
portal → DBus 的前段。日志显示每次呼出的完整链路是:

```
全局快捷键触发:toggle
layer remap: rebuilding surface and GPU presentation
输出回退:重建 GPU surface,等待 configure
呼出高度重设为 1080px → configure 握手完成,重建渲染器
字体重载(JetBrainsMono @32px)→ 网格重算(200×51)
首帧已提交 228ms
```

根因:收起时**必须 unmap 窗口**(KWin 只有见到 Unmapped 才归还焦点,
hardening 轮修复,不可回退),呼出 remap 时当前代码把整条 GPU 栈全部
丢弃重建 —— 包括 `wgpu::Instance`、Adapter、Device、字体系统、glyph
图集与全部管线。用户确认做 1+2:复用可缓存资源 + 首帧先上缓存画面。

## 目标

1. remap 不再新建 Instance/Adapter/Device(罕见的多 GPU 迁移除外,见
   「适配器能力守卫」);字体与 glyph 缓存跨 remap 保留。
2. remap 后 configure 一完成,首帧立即提交上一帧的缓存画面(不等任何重建)。
3. 验收线(双口径,见「测量口径」):真机隔离实例反复 toggle,
   `缓存首帧已提交` p50 ≤ ~120ms(基线 209ms),且 `内容帧已提交` 单独
   上报(观察目标,先测后定,预期不劣于基线);功能零回归;全 workspace
   测试 + 烟测 + clippy/fmt 干净;无新增崩溃。

## 非目标

- 不优化 KWin 快捷键 → portal → daemon 的前段时延(tay 不可控)。
- 不改 unmap 焦点策略(已验证是 KWin 的必要条件)。
- 不做一般性渲染性能工程(帧率/带宽微调)。

## 反目标(失败判据)

- 任何 Mesa WSI / swapchain 段错误回归 —— 仓库历史(hardening-notes
  472、908、1216、1248)教训:旧 wl_surface 由窗口层 `retired` 保活,
  渲染资源必须在 surface 真正销毁前按序释放。builder 审查已核实:
  保留 GpuCore 不改变销毁顺序 —— `wgpu::Surface::drop` 同步释放且
  surface 持有 device 的 Arc,`retired` 在 drain_pending 末尾清理,
  顺序依旧成立。
- 常驻内存无界增长:glyph_buffers 已有 4096 条清空上限(字体/scale
  变化时清空),迁移进 GpuCore 后仍受此约束;缓存帧纹理 ≤ 1 张。
- TAY_SHOT 截图、壁纸、kitty 图像、搜索高亮等功能回归。

## 约束(来自代码事实)

- `create_renderer`(app.rs:671)现在每次新建 Instance(`new_without_display_handle`,
  与 wl_display 无绑定 → 可常驻)→ `request_adapter`(带
  compatible_surface)→ `request_device` → `surface.configure` → 字体
  probe → glyphon `Cache/TextAtlas/TextRenderer/Viewport`(tay-render
  lib.rs:602-699)。其中**只有** surface 本身、format/alpha/尺寸与以
  format 为参数的管线是 surface 绑定的;FontSystem/SwashCache/图集纹理/
  统一缓冲/壁纸/图像缓存都是 device 级。
- glyphon 图集以 surface format 建管线 → 跨 surface 复用需按 format
  隔离;不同输出可能协商出不同 format。
- 新 surface 尺寸可能变化(输出切换/高度重设)→ 缓存帧 blit 需支持
  缩放与钳制(现有 img_pipeline/uv_quad 机制已支持带 rect 的缩放绘制)。
- `SurfaceReplaced`(app.rs:4666)当前 `renderer = None; instance = None;`
  整体丢弃;旧 wl_surface 由窗口层 retired 保活至 drain,这是唯一允许的
  释放顺序。

## 方案(用户确认 1+2)

### 0.先打点、再动刀(计划第一步)

在现有日志基础上补阶段计时:rebind 发起 → configure 到达、旧
Renderer drop 耗时(vkDeviceWaitIdle 可能同步等 GPU)、configure →
首帧、重建各段。真机隔离实例重测基线,得到 209ms 的分段拆解;
后续每步改造都用同一口径复测。若实测旧 Renderer 的同步 drop 显著,
把 drop 移出呼出关键路径(先 stash,新首帧 present 后再释放 ——
retired 保活顺序不变,延后 drop 只会更安全)。

### 1.GpuCore 抽取:device 级资源常驻

- 新增 `GpuCore`(tay-render 内,由 TayApp 持有,替代 `self.instance`):
  Instance + Adapter + Device + Queue + 字体栈(FontSystem/SwashCache/
  Metrics/preedit 缓冲)+ wallpaper/kitty 图像缓存/统一缓冲/quad 缓冲 +
  按 surface format 缓存的 glyphon 集(TextAtlas/TextRenderer/Viewport/
  自绘与图像管线)。
- `Renderer` 收敛为 surface 绑定层:wgpu::Surface + format/alpha/尺寸/
  网格尺寸 + 对 GpuCore 的引用。remap 时只重建这一层。
- `create_renderer` 改造:复用 GpuCore.instance 建新 `wgpu::Surface` →
  协商 format → GpuCore 命中该 format 的 glyphon 集则直接复用(零重建),
  未命中才新建。format 缓存策略:**只保留最近使用的一个 format 集**,
  换 format 时丢弃旧集,防无界增长。
- 字体栈只在 config 字体字段/scale 变化时重载(现有热重载路径已有
  font_changed 判定,复用之)。
- `SurfaceReplaced` 处理:只 drop Renderer(surface 层),GpuCore 保留;
  旧 surface 的 wgpu 资源随 Renderer drop 释放(若第 0 步测得 drop
  显著,则延后到新首帧之后),窗口层 retired 保活顺序不变。

### 2.缓存帧即时上屏

- GpuCore 持有一张 offscreen `cached_frame` 纹理(有界 1 张)。
  **拷贝时机 = 收起时**:收起动画最后一帧渲染完成后、unmap 前,
  `copy_texture_to_texture` 把交换链帧拷入 cached_frame。每个收起
  周期只拷一次,渲染热路径零额外带宽(审查指出的 3840×1188 ≈ 18MB/帧
  的持续拷贝成本被完全消除)。
- remap 后第一帧:configure 完成 → 在 Resized 处理器内**同步**用
  img_pipeline 把 cached_frame 按新 surface 尺寸拉伸 blit 到交换链帧,
  走 `pre_present_notify → present → arm_frame_after_present` 序列
  (绕开从未 present 的 surface 上 KWin 不回应帧回调的 ~50ms 门),
  随后正常重绘覆盖。尺寸不匹配时按比例缩放钳制,边缘可短暂露背景色
  (一帧内被覆盖,可接受)。
- TAY_REBIND/输出回退等无收起场景缓存可能缺失或过期:允许跳过 blit,
  直接走正常渲染首帧,不阻塞。
- TAY_SHOT 模式跳过缓存 blit(截图必须真实内容)。

### 适配器能力守卫(混合 GPU 保险)

rebind 后若 `surface.get_capabilities(&adapter)` 为空(输出迁到其他
GPU、混合显卡机器),放弃复用:为该 surface 重建 adapter + device +
format 集。这是「remap 不再新建」的唯一例外,属稀有路径;常见路径
(单 GPU)不受影响,且避免当前实现按 surface 选适配器的能力回退。

### 测量口径(双时间戳)

- `缓存首帧已提交 Xms`:remap 后第一次 present(blit 或跳过 blit 时的
  正常首帧)—— 用户第一眼看到的画面,验收线 p50 ≤ ~120ms 以它为准。
- `内容帧已提交 Xms`:全内容正常渲染路径的首次 present(沿用现有
  `首帧已提交` 日志语义,保持与 209ms 基线可比)—— 单独上报观察,
  先测后定目标。

## 验证

- **单测分层**:
  - CI(无 GPU,ci.yml 在 ubuntu-latest 跑 workspace 测试):纯逻辑测试 ——
    cached_frame blit 的 src/dst rect 计算(尺寸变化缩放/钳制)、
    format 缓存淘汰策略、阶段计时状态机。尽量设计成无 device 依赖。
  - 本地 GPU 生命周期测试(默认 `#[ignore]`,CI 不跑):跨两次 Renderer
    重建保持同一 Device;glyph 缓存命中(不重跑字体 probe,测试计数器
    断言)。随打包 check 或验收脚本在真机执行。
- **真机隔离实例**(XDG_RUNTIME_DIR+XDG_CONFIG_HOME 指向临时目录,
  WAYLAND_DISPLAY=/run/user/1000/wayland-1,不碰用户 daemon):
  第 0 步的分段基线;TAY_REBIND 强制 rebind + 脚本化反复 toggle N 次,
  双口径统计对比基线;重复 rebind 压力下无 coredump。
- **回归**:panes/focus/desktop-services 烟测、全 workspace 测试、
  clippy/fmt;TAY_SHOT 截图与 baseline 视觉比对。
- 打包:pkgrel 51 → 52,按仓库惯例提交与构建。

## 留给实现者

- GpuCore 与 Renderer 的具体切分边界(哪些字段留在 surface 层)。
- 字体栈「变化才重载」与 glyphon Viewport 重绑定的细节。
- cached_frame blit 的缩放采样参数与「跳过 blit」的判定顺序。
- 与 background_blur(KDE 协议,按 surface 重申请)的交互顺序。
