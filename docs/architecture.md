# Tay 架构设计(第八轮:架构设计优先的重构与加固)

> 2026-10-19。本文记录**核实过的当前事实**与**目标架构**,以及分阶段
> 迁移方式与验收条件。实施进度在 docs/hardening-notes.md 第八轮。
> 原则:在现有 crate 内建立模块边界;不为未知后端设计 trait 体系;
> 每阶段保持可构建、可测试;不一次性大规模重写。

## 1. 当前事实(已核实)

### 1.1 线程拓扑(当前事实,2026-10-19 第十五轮)

| 线程 | 职责 | 与外界的通道 |
|---|---|---|
| UI(主线程,`tay_window::run`) | 事件循环、wgpu 渲染、视图状态(折叠/选区/QuickSelect)、配置、可见性;**不接触 Term** —— 渲染读快照,操作发命令 | wake 管道、term 事件 mpsc、快照单槽 |
| PTY/session 线程(`ptyloop::run`) | **独占 Term**;poll PTY + wake 管道 + 命令 mpsc;协议解析(CountingHandler → Term)、语义状态(命令块/图像/锚点)全部在此维护 | `SessionCommand` mpsc + wake 管道(输入侧);快照单槽 + term `Event::Wakeup`(输出侧) |
| cfg-watch 线程 | notify 监视 config.lua → 写 wake 管道 | wake 管道 |
| IPC accept 线程 | UnixListener accept → `IpcCommand` mpsc | mpsc |
| 热键 portal 线程 | GlobalShortcuts portal → mpsc | mpsc |

### 1.2 跨线程消息(当前事实)

```
UI ──SessionCommand{Input,Resize,ScrollDisplay,ScrollToAnchor{anchor,
       epoch},ToggleVi,ViMotion,Search,CopyAnchors{范围,epoch,应答},
       SelectionToString,Shutdown}──▶ session
UI ◀─SessionUpdate{snapshot, revision}(单槽原子发布:网格 + 语义状态
       同版本;慢 UI 下旧显示快照被覆盖,语义状态在会话侧累积)── session
UI ◀─Event{Wakeup,Bell,Title,ChildExit,ClipboardStore,PtyWrite}── session
UI ◀─SessionReply{Copy{text,revision,stale},Search,Selection}── session
UI ◀─IpcCommand{toggle,show,hide,quit,pin,…}── IPC
UI ◀─热键 mpsc / cfg wake 管道
```

输入背压(统一预算,2026-10-19 第十七轮定点修复):**一个共享
原子计数器**同时覆盖「跨线程消息」与「PTY 待写队列」——
`TermSender::send` 在入队前**原子检查 + 预留**(fetch_update,
多发送者并发不突破上限),超限显式拒绝整块(不静默截断、
不破坏粘贴边界);额度只在字节**实际写入 PTY、明确拒绝或
会话结束**时释放(出队不释放,会话端不再做第二次预算检查);
控制命令(Resize/Shutdown 等)不受预算影响;协议应答
(PtyWrite / kitty 应答)必须送达,不受上限约束但计入在途,
超过「上限 + 256 KiB 余量」告警(查询风暴诊断,不拒绝)。
三种状态明确区分:**已发送**(进通道)/ **已接受**(send 返回
true,保证按序完整写入或显式释放)/ **已写入**(实际写进 PTY)。
UI 对拒绝给出可见反馈(status toast)。
resize 处理:连续 Resize 合并为最新值(`ResizeMerger` 纯逻辑),
遇其它命令先应用 —— 命令 FIFO 保持(后续复制/搜索/输入看到
新布局与失效世代),连续的 configure 风暴只重排一次。
清屏检测:复用内核同源 vte 解析器(跨 flush 状态),文本无 ESC
且无未完成序列时走快速路径跳过双解析(`text_ends_in_escape_
sequence` 保守判定)。

### 1.3 状态所有者(现状)

| 状态 | 当前所有者 | 问题 |
|---|---|---|
| `Term`(网格、光标、模式、selection) | **UI 与 session 共享** `FairMutex` | UI 直接修改(16 处 lock;resize/滚动/vi/选区) |
| `cum_scroll`(累计滚动位移) | **app 字段**(UI 侧) | 与网格分属两次 lock,存在「新网格 + 旧 cum」的单帧不一致窗口 |
| 命令块 `blocks`(元组 `(i32,i32,Option,Option)`) | app | 含义隐晦;无稳定 ID;绝对行 + app 侧手工 `± cum_scroll` 转换散落 10 处 |
| kitty 图像(解码 RGBA + 放置位置) | app | 资源与放置同存;总量预算只有条数(64) |
| 配置 | app(唯一) | 无效热重载保留旧值 ✓;字段即时性靠注释 |
| 可见性/动画 | app(`VisibilityController`) | 唯一 ✓ |
| surface 状态 | tay-window(`configured`/`surface_configured_once`/`frame_pending`/`frame_armed_at`/`stalled`) + app(`sizing`/`renderer_pending`) | 7 个布尔横跨两层,生命周期阶段不可见;无 SurfaceGeneration 校验 |
| 输入写队列 `pending`(VecDeque) | session | 无预算上限 |

## 2. 目标架构:六职责与边界

模块边界在**现有 crate 内**建立(crates/tayd 下分模块;tay-window、
tay-render 保持独立 crate)。不新增 trait 抽象。

### 2.1 App(app.rs)
事件路由、模块组装、平台服务协调(剪贴板、IPC、portal、配置监视)。
**不解析协议、不直接操作终端网格** —— 网格访问一律经 Session 接口。

### 2.2 TerminalSession(session.rs ← ptyloop.rs)
统一管理 PTY、终端内核、协议解析、命令块、图像资源与锚点。
不依赖窗口、GPU、系统剪贴板或浏览器。
- 命令:`SessionCommand::{Input, Resize, ScrollDisplay, Select, Copy{范围,应答}, Search, ToggleVi, …}`(输入侧 mpsc);
- 事件:`SessionEvent::{Wakeup, Bell, Title, ChildExit, ClipboardStore, PtyWrite}` + 语义事件(块/图像/失效);
- **快照**:`SessionSnapshot`(可见区域内容 + 锚点 + `SessionRevision`)。

### 2.3 TerminalView(✅ view.rs,2026-10-19 落地)
可视区域、折叠、选区投影、搜索展示、QuickSelect。
不修改终端底层内容;坐标映射(正向/反向)唯一实现在此。

### 2.4 InputRouter(input.rs ← keys.rs + app 按键分发)
区分:`LocalAction`(tay 快捷键)/ `TermKey`(转义序列)/ `Ime` /
`Paste`。本地模式(vi 复制、QuickSelect、IME 预编辑)与终端输入
的优先级明确:**本地模式全量消费,不进 PTY;正常模式本地快捷键
优先,其余翻译后进 PTY**。release 只影响本地按下语义,不向 PTY
发送。纯决策函数 + 单测。

### 2.5 WindowController(tay-window/lib.rs + app 的 surface 路径)
显隐动画、输出选择、焦点策略、surface 生命周期。
生命周期阶段显式化(见 §5),与 TerminalSession 解耦。

### 2.6 Renderer(tay-render)
消费显示数据(单元、徽章、选区、图像纹理、光标)。
不解释终端协议、不维护业务锚点;图像只认**共享资源句柄**。

## 3. 状态唯一所有者(目标)

| 状态 | 唯一所有者 | UI 如何访问 |
|---|---|---|
| `Term` + `SessionMeta{cum_scroll, revision}` | **session 线程**(迁移终点;本轮先合入同一把锁,见 §6 阶段 2a) | 快照 / 命令应答 |
| 命令块、图像放置、锚点 | session(块事件携带**绝对行**;UI 不做转换算术) | 语义事件 + 快照 |
| 图像解码资源 | session(资源句柄:一次解码、共享引用) | 快照携带句柄 |
| 配置 | App | 只读快照 |
| 可见性/动画 | App(VisibilityController) | — |
| surface 生命周期/帧调度 | tay-window(显式状态机) | — |
| 系统剪贴板 | App 的平台服务(持久 arboard 实例) | — |

## 4. 坐标模型与转换(唯一实现点:session.rs / view.rs)

### 4.1 类型

```rust
/// 历史绝对行:grid 行 + cum_scroll,只增不减的稳定行号。
pub struct HistoryAnchor(pub i32);
/// 网格行(可为负 = 滚动回看)与列。
pub struct GridPoint { pub line: i32, pub col: u32 }
/// 屏幕显示行(row 0..rows)与列。
pub struct DisplayPoint { pub row: usize, pub col: usize }
/// 会话版本:每次 PTY 输出批次推进 +1。
pub struct SessionRevision(pub u64);
/// 锚点失效世代:清屏/重置、resize/reflow、主/备屏幕切换时 +1;
/// 锚点请求(复制/跳转)必须携带来源快照的世代,会话执行前校验。
/// **普通输出滚屏不推进世代** —— 稳定锚点在持续输出下仍可复制。
pub struct AnchorEpoch(pub u64);
/// surface 世代:每次重建 layer surface +1。
pub struct SurfaceGeneration(pub u64);
```

### 4.2 转换(全部集中在此,禁止 UI 手工加减)

| 转换 | 公式 | 失效条件 |
|---|---|---|
| `HistoryAnchor → GridPoint` | `line − meta.cum_scroll` | 锚点早于历史淘汰 → **显式失效**(不猜位置) |
| `GridPoint → DisplayPoint` | `row_map[grid]`,折叠占位行映射到占位标记行 | grid 不在可视范围 → 不渲染 |
| `DisplayPoint → GridPoint` | `row_map` 反查(鼠标命中/光标/选区共用) | 折叠占位行 → 返回区间首行 + `FoldMarker` 标记 |
| `HistoryAnchor → DisplayPoint` | 前两者复合(快照内一次性完成) | 同上 |

### 4.3 失效规则(使锚点失效,不显示错误位置)

| 事件 | 处理 |
|---|---|
| 清屏/重置(ED2/ED3/RIS) | 块与图像锚点**全部失效**(`Invalidated`)+ 失效世代 +1 |
| resize/reflow | 全部失效(行号重排)+ 失效世代 +1(同尺寸 resize 不推进 —— surface 重建场景) |
| 主/备屏幕切换(ALT_SCREEN 进出) | 失效世代 +1(坐标身份切换;主屏网格在 alt 期间冻结,块/图像锚点本身保持有效) |
| 历史淘汰(锚点早于 `−meta.cum_scroll` 裁剪线) | 裁剪该锚点(block 首行、图像) |
| 无法可靠映射的滚动(如顶部锚定局部区域 0..k<rows 的 alacritty 内部整体 rotate) | 按规则计 0(局部操作不是全局平移);如需精确 → 将来内核补丁,本轮不 vendor |
| alt-screen 内的 133 标记 | 抑制(备用屏幕语义隔离) |

**锚点世代(第十七轮定点修复)**:选择、复制、跳转等依赖历史锚点
的请求**携带来源快照的失效世代**(`AnchorEpoch`),会话执行前校验:
- 不匹配 → 明确返回过期(`SessionReply::Copy { stale: true }`),
  UI 轻量反馈(status toast)且**不覆盖已有剪贴板**;
- 世代匹配但锚点已被历史淘汰 → `stale = false` 且 `text = None`
  (Evicted),同样只反馈不复制;
- **普通输出滚屏不推进世代**(不以 revision 差异作粗略拒绝),
  持续输出下选择复制照常工作 —— 竞态窗口(选择→清屏→复制请求
  到达会话前)由世代校验关闭。

### 4.4 内核变更检测(CountingHandler 的定位)

- 优先用内核已有接口:`Dimensions::total_lines` 的增量(历史增长)
  与 `cum` 在历史上限**以下**时互相印证(测试断言);
- 历史上限**之后**内核不暴露单调计数(total_lines 封顶、storage
  重编号不可观测),影子计数是唯一可行机制 —— **保留**,原因与
  升级成本记入 §6 阶段 2c;不 vendor 内核补丁(0.26 升级时优先
  换用内核暴露的滚动计数,若 alacritty 提供)。
- 影子语义与内核逐条对齐并已有回归测试:DECAWM、宽字符、局部
  滚动区、行数截断、备用屏幕、历史淘汰(第七轮)。

## 5. surface 生命周期与资源释放顺序

### 5.1 阶段状态机(tay-window + app 各一份枚举)

```
Uninitialized → AwaitingConfigure → Ready
Ready → ReplacingSurface → AwaitingConfigure → Ready
任意阶段 → Closing
```

- 只有**当前 generation** 完成 configure 后才允许呈现;
- configure / frame / closed 回调一律校验 `SurfaceGeneration`,
  旧世代事件不得修改新实例;
- 输出移除 → `ReplacingSurface`:新 wl_surface + 重绑 layer 角色,
  PTY 会话保持;新 surface **不继承** stalled;
- 退役 surface 释放条件:GPU 资源(vkDestroySwapchain 的 marshal)
  完成后随旧 `WlSurface` 一起 drop(保活于 `retired` Vec,
  不长期积累 —— 每次重建只退役一代);
- 显隐动画(VisibilityController)与生命周期**分状态管理**,
  动画中 surface 重建会重新锚定 margin;
- 隐藏期间调整高度:尺寸与 margin 同一次 commit 提交(已有,
  本轮纳入回归测试)。

### 5.2 初始化 / 退出顺序(已核实,保持)

初始化:主线程 IPC 绑定 → **PTY fork(所有后台线程之前)** →
IPC accept 线程 → 热键 portal 线程 → cfg-watch 线程 → 事件循环。
fork 之后不再有 fork 路径(热键/监视/IPC 均为纯线程)。

退出:IPC quit → `Ctx::exit` → 事件循环收尾 → `Msg::Shutdown`
→ session 线程退出(子进程由 PTY 关闭触发 ChildExit)→ drop
Term/SessionState → 退役 surface 随窗口层 drop(swapchain
marshal 完成)→ arboard 剪贴板实例随 App drop。

## 6. 分阶段迁移与验收条件

每阶段结束:fmt / test / clippy / git diff --check 全绿,可构建。

### 阶段 1(本轮已完成)架构文档与边界
- 验收:docs/architecture.md 齐备(本文);DESIGN.md 同步。

### 阶段 2 会话状态原子化 + 类型化锚点(本轮)
- 2a `SessionState{term, meta{cum_scroll, revision}}` 合入**同一把
  FairMutex**:cum 从 app 移入;块事件携带绝对行;UI 渲染在
  `drain 块事件 → lock 状态` 后读取,网格与锚点必然同版本。
  **不变量:任何一次 grid 读取都配对产生它的 meta**(同锁)。
- 2b 类型化:`HistoryAnchor / GridPoint / DisplayPoint /
  SessionRevision / SurfaceGeneration`;转换集中 session.rs/view.rs;
  删除 UI 侧全部 `± cum_scroll` 手写算术。
- 2c CountingHandler 影子:保留(原因见 §4.4),范围不变,文档化
  升级成本;新增「分块等价」测试用真实 alacritty 网格。
- 验收:全量测试;新增测试断言「任意切分字节流 → 网格内容、
  cum、模式、块事件完全等价」。

### 阶段 3 一致快照与 TerminalView(✅ 会话线程迁移已完成,2026-10-19)
- ✅ session 线程**拥有** Term 与**协议语义状态**(命令块/图像/
  锚点,COW Arc);UI 经 `SessionCommand`(Input/Resize/ScrollDisplay/
  ScrollToAnchor/ToggleVi/ViMotion/Search/CopyAnchors/
  SelectionToString/Shutdown)操作,渲染读 `SessionSnapshot`
  (可见区 + 锚点 + revision + invalidate_epoch + 模式 + 颜色表 +
  块/图像共享引用;只复制可见行,不复制全历史);
- ✅ 快照**单槽原子发布**(`SessionUpdate{snapshot, revision}`):
  语义事件在会话内直接应用,不存在「旧网格配新事件」;
  慢 UI 下旧显示快照被覆盖,语义状态在会话侧累积、不积压队列;
- ✅ 复制/搜索应答携带 `SessionRevision`(复制文本+版本落剪贴板,
  搜索命中记日志);
- ✅ 折叠/徽章/图像/跳转/复制全部经 `SessionMeta::resolve` 唯一
  转换点;顺手修复:图像折叠隐藏此前把绝对行与 grid 相对区间
  比较(滚动后折叠隐藏失效);
- 未完成:TerminalView 模块化(row_map/折叠/选区投影仍内联在
  app.build_grid,未抽独立模块 —— 逻辑已与快照绑定,抽取纯属
  结构整理)。
- 验收:UI 侧 0 处 term.lock()(✅,16 处全部迁移);快照/复制/
  跳锚点单测用真实网格回放(✅);慢 UI 队列有界(预算测试 ✅)。

### 阶段 4 surface 生命周期与 generation 隔离(本轮)
- 状态机枚举替换 7 个布尔;generation 校验;退役条件文档化;
  动画收敛回归(反向/零时长/取整不变/动画中 resize)。
- 验收:窗口层单测(受控时钟);现有 KDE 实测场景回归。

### 图像资源预算计账表(架构 §6「避免重复计费或漏计」)

| 环节 | 预算 | 实现 | 计费方式 |
|---|---|---|---|
| 编码数据(base64 载荷) | 单块 ≤ 32 MiB,分块累计 ≤ 32 MiB | kitty 解码侧 `MAX_KITTY_CHUNK_BYTES` | 每块按原始载荷字节计,超限丢弃整块 |
| 解码像素(单图) | ≤ 64 MiB RGBA | `decode_payload` 尺寸上限 | 解码时一次性校验 |
| 放置实例 | 数量 ≤ 64 + 像素总量 ≤ 64 MiB | 会话侧 `apply_event` 逐出最旧 | 放置与资源一一对应(同 id 替换,不共享),每张 rgba 只计一次 |
| GPU 纹理缓存 | 按当前可见集保留 | 渲染器 `img_cache`(id, 数据指针)键,帧末 retain | 同一数据零哈希零上传;删除/替换图像的纹理随帧释放 |

### 阶段 5 InputRouter / 配置 / 资源策略(✅ 全部落地,2026-10-19)
- InputRouter 纯决策 + 单测(本地/终端/IME/粘贴,release 语义);
- 输入队列预算(超限**显式拒绝 + 日志**,不静默截断)+ 单测;
- 配置:✅ 期望 vs 实际(config.rs 纯决策 `reload_decision`,
  无效热重载保留有效状态、同版本错误只告警一次)、逐字段
  即时性表在 config.rs 模块文档;高度/padding 触发链已有回归;
- 图像:资源句柄与放置分离(数据一次解码)、像素总量预算;
- 验收:各策略单测 + 现有实机回归。

## 7. 必须验证的架构不变量(测试映射)

| 不变量 | 测试 |
|---|---|
| 同一字节流不同分块 → 等价终端与语义结果 | `chunk_split_equivalence_full_term`(真实网格逐边界切分) |
| UI 使用的网格与锚点来自同一版本 | SessionState 同锁 + revision 单测 |
| 锚点始终可解析,或明确失效 | 历史淘汰裁剪、Invalidated、max_span 测试(已有,补充断言) |
| 依赖历史锚点的请求携带世代,过期明确拒绝 | `stale_epoch_is_rejected_not_miscopied`、`scroll_keeps_epoch_and_copies_original_text`、`evicted_anchor_is_evicted_not_stale`、`resize_advances_epoch_only_on_dim_change` |
| 备用屏幕操作不污染主屏幕历史坐标 | 已有 alt-screen 回归 + `alt_screen_advances_epoch_and_separates_coordinates` |
| 旧 surface 回调不能修改新实例 | generation 单测 |
| 隐藏和 surface 重建不终止 PTY | 实机回归(KDE) |
| 慢速 UI 不造成无界队列或内存增长 | 统一输入预算(原子预留 `reserve_input_is_atomic_under_concurrency`、`accepted_300k_input_is_delivered_fully`、`wouldblock_recovers_and_delivers_accepted_input_in_order`、`partial_writes_release_only_written_bytes`、`sender_accepts_300k_rejects_over_budget_and_keeps_control_free`)、图像预算、写队列测试 |
| 已接受 = 保证按序完整写入或显式释放(不因第二次检查丢失) | `response_bypasses_limit_but_is_accounted` + 上述统一预算测试;实机:TAY_PASTE 307200 字节精确交付、慢消费下超限原子拒绝 + 可见 toast |

## 8. 未验证清单(照实标注,不沿用旧笔记结论)

- 合成器矩阵:仅本机 KDE Wayland 实测;wlroots(sway/niri/river/
  labwc)、Hyprland、GNOME 未实测;
- 锁屏恢复、显示器热插拔(单显示器机器无法实测)、多屏指针定位
  未实测 —— surface 重建路径由协议语义 + 单测覆盖;
- IME:ping-pong 修复基于协议语义(KWin 实测正常),其它合成器
  text-input 行为未验证;
- fish 4.9.3 不再内置 OSC 133(逐字节抓包确认),集成改钩子
  (docs/shell-integration.md 已更新)。
