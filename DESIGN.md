# Tay — Linux 一键呼出终端 · 设计蓝图

> 版本 v0.3 · 2026-10-16 · 状态:M0 已完成(见 §9 与 docs/m0-impl-notes.md)
> 竞品与协议事实均核实至 2026-10,依据见文末「参考资料」。

---

## 1. 调研结论:为什么值得做

**市面上没有一款「以 Rust 编写、Linux 原生、一键呼出为核心卖点」的终端。**

| 竞品 | 语言 | 一键呼出 | 缺口(相对本蓝图) |
|---|---|---|---|
| Warp | Rust | 有(全局热键 + Quake 式窗口) | Linux 实现粗糙:失焦不自动隐藏、X11 依赖支持 EWMH 的 WM、GNOME 多显示器 bug;AI 功能需账号、遥测默认开启 |
| Rio | Rust | 有(`ToggleQuake`) | 全局热键仅 macOS/Win/X11,Wayland 需合成器绑定 |
| WezTerm | Rust | 无内置(#1751 开放多年) | 靠 qurop(仅 X11)/ 合成器脚本 / GNOME 扩展拼凑 |
| JeTTY | Rust | 有(专职下拉终端) | ~5 star 的业余项目,非产品级 |
| Ghostty Quick Terminal | Zig | 有(v1.2+,2025-09) | 仅 Wayland layer-shell,**GNOME 不可用**;无 X11 |
| Yakuake / Guake / Tilda / QTerminal | C++/Py/C | 有(成熟) | 非 Rust;Wayland 多为「能用但非一等公民」 |

**空白点**:Rust 原生、X11 + Wayland 双栈、通过 GlobalShortcuts portal 覆盖 GNOME 48+、以呼出体验为产品本体的下拉终端。Ghostty(2025-09 才做 Linux quick terminal)和 Warp(热键功能但 Linux UX 粗糙)证明需求真实存在,但它们各自留了明确的空档。

## 2. 产品定位

- **一句话**:Tay 是一个 Linux 原生、Rust 编写的下拉终端,一键呼出、极速响应、零配置可用。
- **目标用户**:Linux 桌面重度用户、开发者、tiling/WM 用户;把终端当作常驻工具而非「打开—关闭」应用的人。
- **核心体验**:全局热键按下 → 终端从屏幕顶部滑出(热路径 < 50ms)→ 用完失焦自动收起。
- **非目标(明确不做)**:
  - 不追求 macOS/Windows 首发(架构上不锁死,但 Linux 是唯一的一等公民);
  - 不做 Electron/Web 渲染,不做账号体系,遥测默认关闭;
  - 不做 Warp 式 AI 代理平台(AI 集成为远期开放协议级能力,见 §8.5)。

## 3. 设计原则

1. **呼出速度是产品本体**。常驻 daemon + 隐藏窗口模型,热呼出预算 < 50ms;任何特性不得以牺牲呼出速度为代价。
2. **零配置默认好用**(Ghostty 哲学):开箱即用的配色、字体回退、shell 集成;配置项少而正交。
3. **Linux 原生优先**。直接面向 X11/Wayland 的合成器现实设计集成矩阵(§6),不幻想「跨平台抽象一层了事」。
4. **开放协议 > 私有魔法**。命令分块用 OSC 133 而非 Warp 式私有 shell 注入;图像用 kitty graphics/iTerm2/sixel 而非自定义协议。
5. **渐进增强**。模糊、透明、动画在不可用的环境优雅降级为纯色/无动画,永远可用的核心是「呼出 + 终端」。

## 4. 总体架构

### 4.1 常驻 daemon + 薄客户端

```
                      ┌───────────────────────────────────────────────┐
                      │             合成器 / 窗口管理器                 │
                      │  热键来源: GlobalShortcuts portal / XGrabKey / │
                      │            合成器绑定(sway/niri 等)            │
                      └──────┬────────────────────────────┬───────────┘
                             │ 快捷键事件                  │ layer-shell / toplevel
                             ▼                            ▼
 ┌────────────┐  toggle/show ┌────────────────────────────────────────┐
 │  tay CLI   │ ───────────► │          tayd(常驻 daemon)            │
 │ (薄客户端)  │  Unix socket │ ┌──────────┐ ┌──────────┐ ┌─────────┐ │
 │ 无状态转发  │ $XDG_RUNTIME │ │ 窗口层    │ │ 渲染器    │ │ 终端内核 │ │
 └────────────┘  _DIR/tay-   │ │ winit /  │ │ wgpu +   │ │ alacritty│ │
                 {pid}.sock  │ │ SCTK 手写 │ │ glyphon  │ │ _terminal│ │
                             │ │          │ │ + cosmic │ │ (grid/PTY│ │
                             │ │          │ │ -text    │ │  /selection│ │
                             │ └──────────┘ └──────────┘ └─────────┘ │
                             │  隐藏窗口常驻:呼出 = unmap→map,无重建     │
                             └────────────────────────────────────────┘
```

**组件职责**:

| 组件 | 职责 |
|---|---|
| `tayd` | 常驻进程:持有终端会话(grid + PTY)、渲染器、GPU 上下文、隐藏窗口;监听 Unix socket;systemd user service 随登录启动 |
| `tay`(CLI) | 薄客户端:`tay toggle` / `tay show` / `tay new-tab` / `tay config`;无状态,向 daemon 发 IPC 即退出 |
| 热键层 | 三来源合一(见 §6):portal 会话、XGrabKey、外部 `tay toggle` 绑定,统一汇入 daemon |
| 终端内核 | `alacritty_terminal`:grid、VT 解析(vte)、选区、PTY 管理,不碰渲染 |

**为什么用 daemon 模型**(对标依据):
- Guake 模型证明「隐藏而非销毁」是呼出瞬时的关键;
- foot `--server` + client 实测 ~50–80ms 冷启动,而 daemon 常驻后连这一步都省掉;
- Ghostty 官方推荐路径同样是登录时后台实例 + `+new-window`;
- 附加收益:Wayland 剪贴板内容随进程退出而丢失的问题被 daemon 天然解决(arboard 持有者常驻)。

### 4.2 呼出时序与延迟预算

**热路径**(daemon 已常驻,窗口已预渲染):

```
IPC(≤1ms) → xdg-activation / XMapWindow + raise(≤5ms) → 动画首帧(≤16ms)
```

**验收标准:热呼出端到端 < 50ms,冷启动(daemon 首次启动)< 300ms。**
冷路径仅在登录后首次呼出发生;daemon 的 GTK/窗口初始化开销被吸收在登录时。

### 4.3 崩溃与恢复

- daemon 崩溃 = 会话丢失,但窗口层与终端内核分层后,内核状态可做定期快照(远期:复用 WezTerm mux 思路做 detach/reattach);
- systemd user service `Restart=on-failure`;CLI 在 socket 无响应时自动拉起 daemon(冷启动降级路径)。

## 5. 技术选型

| 层 | 选型 | 理由 / 对标 |
|---|---|---|
| 终端内核 | **alacritty_terminal 0.26**(Apache-2.0) | 事实标准嵌入内核;cosmic-term(GPL-3.0)已证明「GPL 应用 + Apache 内核」组合可行 |
| 渲染 | **wgpu + glyphon + cosmic-text** | cosmic-term 同款组合,GPU 文本渲染的 Rust 事实标准;glyphon 2025 全年活跃(0.8→0.10) |
| 窗口层 | **winit ≥ 0.31**(候选) | 0.31(2026-09 beta)大改版带来 IME 改进;Rio 走 winit fork、WezTerm 走自研 XCB+SCTK——均证明 winit 不够时也有退路 |
| 剪贴板 | arboard(wl-clipboard-rs 后端) | daemon 常驻使 Wayland 剪贴板存活问题消失 |
| Portal | **ashpd** 0.13 | GlobalShortcuts portal 的 Rust 事实标准封装 |
| layer-shell | SCTK + wayland-protocols-wlr 手写 | 仅此一处需要手写 Wayland 协议,量小 |
| 配置 | TOML,XDG 路径,热重载 | 零配置哲学;Lua 作为远期可选 |
| 文本整形 | cosmic-text(harfbuzz 系) | 连字(ligature)、CJK、字体回退、bidi 一站式 |

**关键决策:UI 框架不用 GTK4/iced,理由与代价**

| 方案 | 优点 | 代价 |
|---|---|---|
| GTK4(Ghostty 路线) | IME、无障碍、原生对话框免费 | 初始化 ~300ms(靠 daemon 掩盖)、自定义渲染要跟 GTK 搏斗 |
| iced(cosmic-term 路线) | 上手快 | iced 文本管线非终端级,cosmic-term 最终仍直接改用 glyphon |
| **winit + wgpu(本方案)** | 完全掌控、最低延迟(参考 aileron ADR-004:1–2ms 按键到屏) | IME 需自行处理(`set_ime_allowed(true)` + 预编辑渲染)、无障碍从零做 |

终端是一个「整面墙都是自绘文本」的应用,UI 框架提供的价值最小、摩擦最大;自绘是行业共识(Alacritty/kitty/WezTerm/Rio 全部自绘)。**IME 是最大风险,处理预案见 §10。**

## 6. Linux 桌面集成矩阵(核心难点)

Wayland 上「全局热键」和「窗口定位」都没有统一答案,必须按合成器分治。这是本产品与竞品拉开差距、也是最大工程投入的地方。

### 6.1 全局热键(一键呼出的入口)

| 环境 | 机制 | 状态 |
|---|---|---|
| **KDE Plasma**(5.27+/6) | GlobalShortcuts portal | ✅ 最成熟,ashpd 直接可用 |
| **GNOME 48+** | GlobalShortcuts portal | ⚠️ 后端年轻,`ConfigureShortcuts` 有已知 bug;48 以下无热键途径 |
| **Hyprland** | portal(xdg-desktop-portal-hyprland,0.52.1+ 原生协议) | ✅ |
| **sway / niri / river / labwc 等 wlroots** | 无 portal 后端 → 合成器绑定 `tay toggle` | 文档化引导(首次运行显示配置向导,给出各合成器现成配置片段) |
| **X11** | XGrabKey(daemon 内,root window 抓取) | ✅ 经典路径;注意 `AnyModifier` 冲突陷阱,需按修饰键组合逐个抓取 |

**产品化设计**:首次运行向导 → 尝试 portal 绑定(GNOME/KDE 弹系统确认框)→ 失败则生成合成器配置片段 → X11 直接抓取。Ghostty 已走通 portal 绑定 + 用户确认的流程,可对标。

### 6.2 下拉定位与窗口形态

| 环境 | 方案 |
|---|---|
| wlroots(sway/niri/river/labwc)/ KDE / Smithay | **wlr-layer-shell** overlay 层:真 Quake 下拉,mapping 不触发桌面重排,呼出零成本 |
| **GNOME(Mutter)** | 无 layer-shell(上游明确拒绝)→ 普通 toplevel + xdg-activation + 置顶,配合**官方 GNOME 扩展**(定位 + 动画,对标 ddterm/abn.is 的 Ghostty 方案) |
| X11 | 普通窗口 + `_NET_WM_STATE_ABOVE` + `SKIP_TASKBAR` + `SKIP_PAGER`;ARGB visual 透明;override-redirect 仅作为对抗不友好 WM 的逃生舱 |

### 6.3 失焦自动隐藏

- 语义:呼出 → 使用 → 焦点离开 → 收起。X11 用 FocusOut 可靠实现;
- Wayland:窗口焦点事件随合成器而异,portal 的 `Activated` 信号带 activation token,可用来判断「热键再次按下」切换显隐;
- 默认策略:热键 toggle 永远可靠;失焦自动隐藏做成按合成器调校的配置项(默认开,给出每环境推荐值)——Warp 在 Linux 上至今没有失焦隐藏,这是明确的竞争差距。

### 6.4 模糊与透明(渐进增强)

- KDE:新标准 `ext-background-effect-v1`(KWin 6.7+;旧 blur 协议已移除,Ghostty/Zed 均已被迫迁移);
- 其他:无标准 → 纯透明(ARGB)或实色降级;GNOME 客户端不可请求模糊,靠扩展或干脆实色;
- **设计含义:模糊永远是增强项,永远可降级,不阻塞核心体验。**

### 6.5 多显示器

- X11:RandR/Xinerama 取鼠标所在显示器,呼出跟随;
- Wayland:客户端读不到全局指针位置 → 折中:每个输出各建一个 layer surface(热路径都预渲染),呼出到「最后有焦点窗口的输出」;或提供 `output` 配置。作为已知挑战列入 §10。

## 7. 终端协议支持(分级)

**P0 桌面基准**(第一个可用版本必须):
- bracketed paste、焦点报告(DECSET 1004)、OSC 8 超链接、DEC 2026 同步输出、OSC 133/7 shell 集成、truecolor、DECSET 2027 字素簇聚类(Neovim 0.11 已依赖)。

**P1 差异化**:
- **kitty keyboard protocol**(完整实现;注意 `CSI < u` 退出复位细节——xterm.js 等已证明易踩坑);
- **图像协议三件套**:kitty graphics + iTerm2 inline images + sixel(Rust 侧有 `sixel-rs`/`icy_sixel`/`a-sixel` 可用;kitty graphics 编码参考 Ghostty 的 ~270 行最小实现);
- OSC 133 深度消费:命令分块、退出码徽章、按块复制、提示符跳转(见 §8.4)。

**P2**:OSC 52 剪贴板写、DECRQM/DA2 探测响应、真色之外的颜色空间(display-p3,现状:HDR 在 Linux 终端普遍不成熟,不追)。

**关键洞察**:fish 4.0(2025-02)原生输出 OSC 133,零配置即可获得命令块体验;bash/zsh 提供一键安装的集成片段(kitty/WezTerm/VS Code 片段互通)。**开放协议的「Warp blocks」是现成的,不需要私有 shell 注入。**

## 8. UX 设计

### 8.1 呼出体验
- 顶部滑出,弹性动画(时长可配,Ghostty 同款思路;默认 ~150ms 不拖沓);
- 宽度默认全宽(可配百分比,Rio 有 `quake-width-percentage` 先例);
- 热键默认 `F12`(Quake 传统),portal 环境首次绑定走系统确认框。

### 8.2 终端本体
- CJK 与 IME 一等公民(中文输入、预编辑浮层渲染、死键);
- 连字、Nerd Font 回退、灰度/次像素 AA 双模式;
- 主题系统(内置暗色 + 亮色,TOML 自定义);滚动回退 + 增量搜索 + vim 复制模式(对标 WezTerm copy mode)。

### 8.3 快速选择(QuickSelect)
- 一键触发,为 URL/路径/sha/git hash 等打标签后跳转或复制(WezTerm 有 14 类内置 pattern 可对标);
- 与「呼出即用」的场景天然契合:日志里抓一个链接,三键完成。

### 8.4 命令块(OSC 133 消费侧)
- 每条命令输出为一个块:退出码徽章、折叠、单独复制、跳转到上一条提示符;
- Zellij 0.45+ 已证明这套能力(`SelectCommandAtScrollPosition` 等)对用户价值极高;
- 这是与 Warp 最大的体验对标点,且**全部基于开放协议**。

### 8.5 远期(不入首个版本)
- AI 集成:仅以开放方式做(如 ollama 本地模型的「解释这段输出」),不做账号体系;
- 插件系统:参考 Zellij 的 wasmi/WASM 沙箱方案,或先做 kitty kittens 式的子命令扩展;
- SSH 会话保持 / detach-reattach(WezTerm mux 思路);
- eDEX-UI 风格主题作为社区主题而非默认。

## 9. 里程碑路线图

| 里程碑 | 目标 | 验收 |
|---|---|---|
| **M0 内核就绪** ✅ | alacritty_terminal + winit + wgpu/glyphon 渲染出可用终端 | fish 全功能、CJK 输入、连字、150fps 滚动 —— **实测 avg 0.75ms/帧 ≈ 1339fps**(2026-10-16,KDE Wayland + AMD) |
| **M1 下拉成形(X11)** | daemon 化 + 隐藏窗口 + XGrabKey + 动画 | 热呼出 < 50ms、失焦隐藏、多显示器 |
| **M2 Wayland 覆盖** | layer-shell(wlroots/KDE)+ GlobalShortcuts portal + GNOME 扩展伴侣 | 在 sway、Hyprland、KDE、GNOME 上全部一键呼出 |
| **M3 体验差异化** | OSC 133 命令块、QuickSelect、搜索/复制模式、主题热重载 | fish 零配置命令块;竞品对标清单逐项打勾 |
| **M4 生态** | 图像协议、打包(Flathub/AUR/nixpkgs/COPR)、配置文件向导 | Flatpak 沙箱下 portal 全链路可用 |

## 10. 风险与对策

| 风险 | 影响 | 对策 |
|---|---|---|
| **winit IME 缺口**(Wayland 中文输入) | 对中文用户致命 | 0.31 IME 改进后优先验证;必须 `set_ime_allowed(true)`(Warp 曾因 cfg 门控导致 CJK 静默失效);兜底:自研 SCTK 窗口层(WezTerm 路径)或 winit fork(Rio 路径) |
| **GNOME 环境**(无 layer-shell、portal 后端年轻) | 覆盖不全 | 官方扩展伴侣 + toplevel 降级;对标 ddterm/abn.is Ghostty 方案已验证可行 |
| **alacritty_terminal 无 API 稳定承诺**(pre-1.0) | 升级成本 | 锁定版本 + 升级纳入每个里程碑;远期评估 termwiz 双内核抽象 |
| **合成器矩阵维护成本** | 持续工程投入 | 集成矩阵(§6)作为一等公民维护;每环境 CI 冒烟测试 |
| **模糊协议碎片化**(KWin 刚换协议) | 视觉不一致 | 模糊=增强项,降级为纯色;跟随 `ext-background-effect-v1` |
| **Wayland 多显示器指针定位不可得** | 呼出位置体验 | 每输出预渲染 surface + 焦点输出推断;见 §6.5 |

## 11. 工程结构(Cargo workspace 建议)

```
tay/
├── crates/
│   ├── tayd/            # daemon 主程序 + IPC 服务
│   ├── tay/             # CLI 薄客户端
│   ├── tay-window/      # 窗口层抽象(winit 适配 / SCTK 兜底)
│   ├── tay-render/      # wgpu + glyphon 渲染管线
│   ├── tay-integration/ # portal / layer-shell / XGrabKey / EWMH
│   ├── tay-config/      # TOML 配置、默认值、热重载
│   └── tay-protocol/    # OSC 133 消费、图像协议编解码、QuickSelect
├── extensions/          # GNOME Shell 扩展伴侣
└── docs/
```

许可证:GPL-3.0;终端内核 alacritty_terminal(Apache-2.0)保持其原许可。

## 12. 决策记录(2026-10-16 已确认)

| 决策 | 结论 |
|---|---|
| 窗口层 | **winit ≥ 0.31 起步**;抽象接口按 WezTerm `window` crate 的形状设计,保留换轨自研 SCTK 的余地 |
| 配置语言 | **TOML 起步**;**Lua(WezTerm 式可编程配置)列入远期**,M4 之后评估 |
| 许可证 | **GPL-3.0**(与 cosmic-term 同款;alacritty_terminal 为 Apache-2.0,组合已证实可行) |
| 命名 | **Tay** |

**待真机调校**:失焦隐藏默认值(Linux 上 Ghostty 默认关闭、Warp 缺失——需按合成器实测后再定默认开/关)。

---

## 参考资料(关键来源)

- 竞品现状:WezTerm 下拉请求 [#1751](https://github.com/wezterm/wezterm/issues/1751);Rio [ToggleQuake 文档](https://rioterm.com/docs/key-bindings);Warp [Global Hotkey 文档](https://docs.warp.dev/terminal/windows/global-hotkey/);Ghostty [1.2 发布说明](https://ghostty.org/docs/install/release-notes/1-2-0) 与 [quick-terminal Linux 讨论](https://github.com/ghostty-org/ghostty/discussions/3459);cosmic-term quake 请求 [#244](https://github.com/pop-os/cosmic-term/issues/244);Ptyxis 语言统计(GitLab API)
- 启动性能:Omarchy PR [实测数据](https://github.com/omacom/omarchy/pull/9917)(foot ~50–80ms、Ghostty 常驻化 150–300ms);Ghostty 官方常驻实例建议(讨论 [#8475](https://github.com/ghostty-org/ghostty/discussions/8475))
- 集成协议:[GlobalShortcuts portal 规范](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.GlobalShortcuts.html);[gtk4-layer-shell 支持矩阵](https://github.com/wmww/gtk4-layer-shell);GNOME 下拉 Ghostty 方案(abn.is);KWin blur 迁移 [commit](https://invent.kde.org/plasma/kwin/-/commit/7f9e0aa1cd2c2ab4fea1097800893b6f780089eb)
- 协议生态:[终端兼容矩阵](https://silvery.dev/reference/terminal-matrix);OSC 133 综述(backscroll);fish 4.0 [原生 OSC 133](https://raw.githubusercontent.com/fish-shell/fish-site/master/site/docs/current/terminal-compatibility.html);[grapheme clustering](https://raw.githubusercontent.com/jerrylususu/bookmark-summary/refs/heads/main/202511/2025-11-04-grapheme-clusters-and-terminal-emulators.md)
- 技术栈:aileron [ADR-004](https://github.com/WyattAu/aileron/blob/main/.adrs/ADR-004-native-terminal-emulator.md)(winit+wgpu 延迟实测);winit [0.31 发布](https://github.com/rust-windowing/winit/releases) 与 Warp [IME 修复 PR #9277](https://github.com/warpdotdev/warp/pull/9277);glyphon/cosmic-text/alacritty_terminal 的 docs.rs 与 GitHub 主页
