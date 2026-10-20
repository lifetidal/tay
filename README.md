# Tay

**Linux 一键呼出下拉终端** —— Rust 原生、Wayland layer-shell、零配置可用。

设计蓝图见 [DESIGN.md](DESIGN.md);实施笔记见 [docs/](docs/)
(含 [docs/hardening-notes.md](docs/hardening-notes.md) 加固轮)。

## 特性

- **热呼出 < 50ms**(实测 16ms 首次 / 1ms 常驻):daemon 常驻 + layer
  surface 全程 mapped,呼出只动 top margin,GPU 资源零重建
- **一键呼出**:GlobalShortcuts portal(KDE/Hyprland)或 `tay toggle`
- **失焦自动隐藏**(`tay pin` 可临时固定)、150ms ease-out 滑入动画、每输出独立呼出
- **OSC 133 命令块**(fish 4+ 零配置;bash/zsh 片段见
  [docs/shell-integration.md](docs/shell-integration.md)):退出码徽章、
  折叠(点徽章 / Ctrl+Shift+H)、提示符跳转(Ctrl+Shift+↑/↓)、
  按块复制(Ctrl+Shift+O,只复制纯输出)
- **QuickSelect**(Ctrl+Shift+F):URL/路径/git hash 打标签,按键即复制,
  Shift+标签同时 xdg-open(打开动作与复制分离)
- **vi 复制模式**(Ctrl+Shift+Space):hjkl 移动、`/` 全滚动回退搜索、
  `y` 复制
- **图像协议三件套**:kitty graphics / sixel / iTerm2 inline(PNG/RGBA/RGB)
- **OSC 8 超链接**:链接段下划线,悬停手形指针,**Ctrl+点击** 直接
  xdg-open(与 QuickSelect 的打开语义互补)
- **可视铃**:终端 BEL 时网格背景短闪(140ms 衰减混色)
- **会话恢复**:`tay quit` 优雅退出时保存标签骨架(shell/工作目录/
  标题),`tay resurrect` 恢复全部标签(滚动历史/图像不恢复 ——
  内核状态不可序列化)
- **主题热重载**:改 `~/.config/tay/config.lua` 即时生效
- CJK 与 IME(text-input-v3)、ANSI 真色、滚动性能 >1000fps
- **质量基建**:186 项测试 + proptest 属性测试、cargo-fuzz 双
  harness(协议解析 / sixel 解码)、RustSec 依赖审计、GitHub
  Actions CI(严格 Clippy 门禁)

## 安装

```bash
# Arch Linux：打包当前源码（包含未提交修改）
./tools/package-arch.sh
sudo pacman -U ./dist/arch/tay-*.pkg.tar.zst

# 或源码构建
cargo build --release
install -Dm755 target/release/{tay,tayd} ~/.local/bin/
```

随登录自启(图形会话):

```bash
systemctl --user enable --now tayd.service
```

## 使用

```bash
tayd &          # daemon 常驻(窗口隐藏)
tay toggle      # 呼出/收起(tay show / tay hide / tay quit)
tay pin         # 临时固定:暂停失焦自动隐藏(再发一次解除)
tay resurrect   # daemon 停止后:恢复上次退出保存的标签骨架
```

首次按 **F12** 会在 KDE 弹窗确认一次全局热键绑定。

## 配置

`~/.config/tay/config.lua`(WezTerm 风格:脚本 `return` 一个配置表,
全部字段可选、缺省用默认值;保存后自动热重载):

```lua
return {
  shell = "",           -- 空 = $SHELL(需重启)
  height_percent = 45,  -- 呼出高度占输出百分比(1–100,即时生效)
  animation_ms = 150,   -- 滑入动画,0 = 关闭(即时生效)
  hide_on_focus_out = true,
  start_hidden = true,  -- 需重启
  output = "",          -- 呼出输出名(如 "DP-1");空 = 合成器默认(需重启)
  padding = 3,          -- 单元格内边距(px,即时生效)
  opacity = 1.0,        -- 窗口不透明度 0–1(即时生效)

  font = {              -- 即时生效
    family = "monospace",
    size = 12.0,
  },

  colors = {            -- 即时生效(操作栏自动跟随)
    foreground = "#d4d4d4",
    background = "#0f1114",
    cursor = "#00a884",
    selection = "#395e9c",
  },
}
```

- **即时生效**:`height_percent`(重设呼出高度)、`animation_ms`、
  `hide_on_focus_out`、`padding`(网格重算)、`opacity`、`colors`、`font`
- **需重启 tayd**:`shell`、`output`、`start_hidden`
  (修改后日志会注明「运行中保持 <当前值>」)
- 热重载失败(语法错误、类型错误、越界值如 `opacity = 2`)时
  **沿用上次有效配置**,日志报错;修好文件后自动恢复;删除配置文件 =
  保持当前配置(重启后回默认)。越界规则:`height_percent` 1–100、
  `opacity` 0–1、`animation_ms` 0–5000、`padding` 0–100、`font.size` 1–200,
  `nan`/`inf` 一律拒绝。
- 配置脚本在**沙箱**中求值:仅安全标准库(无 `os`/`io`/`debug`/
  `package`)—— 配置不应有副作用。
- 旧 `config.toml` 首次启动自动迁移为 config.lua，原文件归档为 `.migrated.bak`;
  `tay config` 生成模板。
- 界面修改的字体、字号和配色写入 `config.lua` 末尾的 Lua 设置区，原始脚本逻辑和注释保留在函数内。第一次修改会保存 `config.lua.before-ui.bak`。
- Lua 配置有执行指令和内存预算；热重载在后台执行，失败时保持当前终端。

## 开发

验证环境变量:`TAY_SHOT=路径[,帧数]`(截图退出)、`TAY_BENCH=帧数`
(滚动基准)、`TAY_EXEC=命令`(启动后执行)、`TAY_START_VISIBLE=1`、
`TAY_SHELL=路径`。

```bash
cargo test                 # 144 个单元测试(架构不变量与回归)
cargo run -p tayd          # 交互体验
```

## 架构

```
crates/
├── tay/             CLI 薄客户端(Unix socket IPC)
├── tayd/            常驻 daemon:终端内核(alacritty_terminal)+
│                    自研 PTY 事件循环(OSC 133/图像协议流拦截)+
│                    portal 热键 + 呼出状态机。模块边界(第八轮):
│                    session.rs(Term + SessionMeta 同锁、类型化锚点)、
│                    blocks.rs(CommandBlock/BlockId 命令块存储)、
│                    input.rs(InputRouter 按键/IME/粘贴路由纯决策)、
│                    ptyloop.rs(会话线程、CountingHandler 精确计数)
├── tay-window/      手写 SCTK + wlr-layer-shell 窗口层
│                    (SurfacePhase 生命周期状态机 + 世代校验)
├── tay-render/      wgpu + glyphon 渲染管线(网格/徽章/overlay/图像)
└── tay-config/      Lua 配置(沙箱求值,TOML 自动迁移)
```

完整架构(线程、状态所有者、坐标模型、失效规则、迁移计划):
docs/architecture.md。

许可证:**GPL-3.0**(终端内核 alacritty_terminal 为 Apache-2.0)。

### 标签页与外观设置

顶部标签的 `x` 关闭该会话；关闭最后一个标签会隐藏窗口，后台保留呼出快捷键，下次呼出创建新会话。右上角 `—` 隐藏并保留会话，`□` 最大化/还原高度，`×` 关闭窗口并结束全部会话。标签后的 `+` 新建独立 Shell。

- `Ctrl+Shift+T` 新建，`Ctrl+Shift+W` 关闭，`Ctrl+Tab` / `Ctrl+Shift+Tab` 切换。
- `Ctrl++` / `Ctrl+-` 或 `A+` / `A-` 调整字号；`Ctrl+0` 恢复 Lua 配置中的基础字号。
- `Ctrl+,` 或齿轮按钮 打开设置。点击 `Font`、`Background`、`Text color` 编辑；`Ctrl+A` 清空原值，回车保存，Esc 取消。颜色格式为 `#RRGGBB`，推荐使用已安装的等宽字体，通用字体名可填写 `monospace`。
- 外观立即生效并保存至配置文件。
- 全局呼出使用桌面的 GlobalShortcuts portal，默认申请 F12；需在系统授权窗口确认。`Hotkeys` 显示授权状态，KDE 下同时打开快捷键设置。若桌面未提供该 portal，可在系统快捷键中绑定 `tay toggle`。窗口有焦点时 F12 可直接隐藏。

标签条采用加高布局，文字与图标垂直居中，展开设置后显示额外控件。Lua 配色与布局保存后自动热重载：

```lua
return {
  theme = "midnight",
  tab_bar = {
    visible = true,        -- false = 极简模式(栏隐藏、终端占满全高;
                           --   设置面板展开时临时显示;快捷键不受影响)
    compact = true, tab_width = 24, -- 12–40 个显示列
    background = "#111d30", active_background = "#246fba",
    active_foreground = "#ffffff", inactive_foreground = "#b2c6e0",
    hover_background = "#263c59", accent = "#83d7ed",
    clock_background = "#244155", clock_foreground = "#cae7ef",
    icon_foreground = "#93a8be", icon_hover_foreground = "#a4dce8",
    danger_foreground = "#ee9fa7",
    -- 颜色均可省略:未显式配置的 tab_bar 颜色从主题自动推导
    -- (换 theme 时操作栏自动协调,推荐省略全部颜色)。
  },
}
```

标签使用左上、右下圆角（其余直角）的分离形状与粗体，设置按钮为矢量齿轮，时钟独立预留位置，配色可通过上述 Lua 字段修改。右缘滚动条支持拖动滑块和点击轨道定位；备用屏幕应用中不显示历史滚动条。

完整冰蓝预设见 [config/desktop.lua](config/desktop.lua)，包含字体、透明度和快捷键。

命令行也支持 `tay new-tab`、`tay next-tab`、`tay close-tab`、`tay settings`、`tay maximize`、`tay restore`。

可用性回归：先运行 `cargo build --workspace`，再在 Wayland 桌面运行 `python3 tools/usability-smoke.py`。它使用临时配置和独立 socket，验证冷启动、标签生命周期、关闭最后标签后重新呼出、字体热更新，并输出截图路径；不会替代物理键盘与系统全局快捷键的人工验收。CLI 拉起后台的日志保存在 `$XDG_RUNTIME_DIR/tayd.log`。


### Lua 快捷键、主题与透明背景

日常预设见 `config/desktop.lua`。预设使用参考本地 WezTerm 的自定义配色、12pt Medium JetBrainsMono Nerd Font、94% 背景不透明度及 Ctrl+grave 全局呼出。已有用户配置优先，不会在安装包升级时被替换。

```lua
return {
  theme = "midnight", -- default / midnight / light
  opacity = 0.94,
  font = { family = "JetBrainsMono Nerd Font", size = 12.0 },
  global_shortcut = "CTRL+grave", -- 修改后重启，并在系统授权界面确认
  keys = {
    { key = "t", mods = "CTRL|SHIFT", action = "new-tab" },
    { key = "PageDown", mods = "CTRL", action = "next-tab" },
    { key = "v", mods = "CTRL|SHIFT", action = "paste" },
  },
}
```

`keys` 按组合键覆盖默认窗口快捷键，其余默认绑定保留；设置 `disable_default_key_bindings = true` 可完全自定义，修饰键精确匹配。支持 `CTRL|SHIFT|ALT|SUPER`，无修饰键可用 `NONE` 或空字符串，动作包括 new-tab、close-tab、next-tab、previous-tab、tab-1…tab-9、copy、paste、settings、hide、maximize、font-increase、font-decrease、font-reset（恢复 Lua 基础字号）及 none（屏蔽绑定）。原有高级复制模式/命令块快捷键仍可用，可用 none 覆盖。

窗口内快捷键、主题、透明度和字体热重载；全局呼出键需要重启及桌面授权，实际按键以系统授权结果为准。背景透明需要合成器支持透明表面，文字与控件保持清晰；尚未实现背景模糊。`colors` 可覆盖主题的单个颜色，`colors.ansi` 可提供 16 色调色板，256 色索引也会正确解析。

配置统一为 `config.lua`。界面按字段写入 Lua；例如只调整字号不会固定主题配色。已有 `appearance.toml` 会一次性合并到 Lua 后归档，不再作为覆盖层读取。


### 快捷键与 WezTerm 参考主题

`tay keys` 列出当前配置解析后的主要窗口快捷键（包括默认继承和自定义覆盖）。
新建标签支持 Ctrl+T / Ctrl+Shift+T / Alt+T，关闭标签支持 Ctrl+W / Ctrl+Shift+W，
切换支持 Ctrl+Tab / Ctrl+Shift+Tab、Ctrl+PageDown / Ctrl+PageUp、Alt+[ / Alt+]，
Alt+1…9 直达标签；复制/粘贴为 Ctrl+Shift+C / Ctrl+Shift+V，Shift+Insert 也可粘贴。
Ctrl+= / Ctrl+Shift++ 放大，Ctrl+- 缩小，Ctrl+0 恢复 Lua 中的基础字号。
Ctrl+, 打开设置，Ctrl+Shift+M 切换最大高度。Ctrl+C、Ctrl+V 和 Ctrl+M 仍传给终端程序。
如需把 Ctrl+T/W 留给 Shell，可在 keys 中覆盖为其他动作，或完全关闭默认绑定后自定义。
`none` 会消费组合键；需要透传时使用 `disable_default_key_bindings = true` 并仅列出需要的窗口绑定。

```lua
return {
  theme = "wezterm",
  font = { family = "JetBrainsMono Nerd Font", size = 12, weight = 500 },
  opacity = 0.94,
  -- 默认绑定继续可用，下面仅覆盖一条。
  keys = { { key = "F12", mods = "NONE", action = "hide" } },
}
```

`wezterm` 是本项目内置的配色预设，参考本地 WezTerm 的自定义 Catppuccin：
背景 #1f1f28、前景 #cdd6f4、光标 #f5e0dc，以及对应的 ANSI 16 色和标签配色。
`font.weight` 支持 100–900（400 Regular、500 Medium、700 Bold），会同步用于测量与渲染。
该预设不执行 WezTerm 脚本；其壁纸、模糊、字体栅格化和高级交互效果没有直接移植。


fish / Starship 提示符回归：构建后运行 `python3 tools/fish-smoke.py`，
或追加 `--starship-config ~/.config/starship/starship.toml` 检查自己的 Starship 配色。
测试使用独立 fish 配置、历史目录与 socket，输出截图和日志路径。
Nerd Font 图标可使用紧邻的同背景空格；紧邻文字或光标时按字形墨迹缩放适配，
不会改变终端列位置。Unicode 组合重音随快照保留，并与基础字符一起塑形。


Lua 文件末尾的手写设置会在界面保存时保留。`TAY_UI_FIELDS_BEGIN/END`
之间只放界面管理的外观赋值；可手动调整这些值，其他逻辑请放在标记区之外。
若标记区包含其他代码，界面会拒绝写入并提示，保留文件原文。
旧配置归档失败只告警；无法写入的外观迁移会保留旧文件并延后重试。
