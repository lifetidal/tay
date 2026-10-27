# Tay

<img src="packaging/icons/tay.png" alt="Tay 项目 Logo" width="160">

**按一下按键,终端就从屏幕顶部滑下来;再按一下,它就藏回去。**

一个 Linux 下拉终端(像老游戏里的 Quake 控制台)。Rust 写的,跑在
Wayland 上,没装任何配置也能直接干活。常驻后台，实际呼出速度取决于桌面合成器、显卡和机器配置。

设计蓝图见 [DESIGN.md](DESIGN.md);开发过程记录见 [docs/](docs/)。

## 装

**Arch Linux**(打包脚本会用当前源码打出 .pkg.tar.zst):

```bash
./tools/package-arch.sh --offline
sudo pacman -U ./dist/arch/tay-0.1.0-39-$(uname -m).pkg.tar.zst
```

或者直接从源码装:

```bash
cargo build --release
install -Dm755 target/release/{tay,tayd} ~/.local/bin/
```

开机自动常驻(可选):

```bash
systemctl --user enable --now tayd.service
```

## 上手三分钟

```bash
tayd &          # 启动常驻进程(窗口默认藏起来)
tay toggle      # 呼出 / 收起
```

之后按 **F12**(首次按 KDE 会弹一次窗,确认绑定)就能呼出。
不需要了:`tay quit` 退出。

进去以后就是一个普通终端:**敲命令、滚轮回看历史**。想复制?

- 鼠标拖一段 → 按 `Ctrl+Shift+C`(拖选高亮在,复制的内容就是
  你选中的文字,滚屏也不跑偏)
- `Ctrl+Shift+V` 粘贴

想要多个终端?**顶部标签栏**帮你管:

| 想做什么 | 按什么 |
|---|---|
| 开个新标签 | `Ctrl+Shift+T` |
| 关掉当前标签 | `Ctrl+Shift+W` |
| 切换标签 | `Ctrl+Tab` / `Ctrl+Shift+Tab` |
| 直达第 N 个标签 | `Alt+1` … `Alt+9` |
| 呼出/收起 | `F12`(或 `tay toggle`) |
| 暂时别自动隐藏 | `tay pin`(再发一次解除) |

标签栏右上角的 `—` 收起窗口(会话保留),`□` 把高度拉到最大/
还原,`×` 关掉整个 Tay。标签上的小 `x` 只关那一个标签。
命令行也有:`tay new-tab`、`tay next-tab`、`tay close-tab`、
`tay maximize`、`tay restore`、`tay settings`。

## 原生分屏

一个标签里也可以并排显示多个独立终端，支持嵌套分割：

| 操作 | 默认快捷键 | 命令 |
|---|---|---|
| 向右分割 | `Ctrl+Shift+D` | `tay split-right` |
| 向下分割 | `Ctrl+Shift+E` | `tay split-down` |
| 切换到下一个窗格 | `Ctrl+Alt+O` | `tay next-pane` |
| 关闭当前窗格 | `Ctrl+Alt+W` | `tay close-pane` |
| 当前窗格最大化 / 还原 | `Ctrl+Shift+Z` | `tay zoom-pane` |

点击窗格切换焦点，拖动分隔线调整大小。新窗格沿用当前 shell 和工作目录；
输入、滚动、选区、搜索和折叠状态各自独立。标签标题中的 `[2/3]` 表示
当前是第 2 个窗格，共 3 个。关闭标签会结束其中全部窗格。

每个标签最多 8 个窗格，全部标签合计最多 24 个会话。窗口过小时暂时只显示
焦点窗格，放大后恢复布局。布局和工作目录每秒采样，由后台线程合并保存；正常退出会保存最终状态。
异常退出后下次启动自动恢复，正常退出后可用 `tay resurrect` 恢复。
只恢复 Shell、目录和布局，不恢复运行中的命令与滚动历史。

快捷键可在 Lua `keys` 中覆盖，action 名称与上表命令相同。

## 让它好看

所有外观都在一个文件里:`~/.config/tay/config.lua`。
外观改完保存后立即生效；Shell 对新会话生效，全局快捷键需要重启并授权。写坏了也没关系——它会
自动沿用上一次能用的配置。

```lua
return {
  theme = "midnight",     -- 内置主题:default / midnight / light / wezterm
  opacity = 0.94,         -- 背景透明度:1 完全不透明(文字始终清晰)
  font = { family = "JetBrainsMono Nerd Font", size = 12.0 },
  padding = 8,            -- 四周留白,大一点更透气

  colors = {              -- 也可以只覆盖主题里的某个颜色
    cursor = "#58a6ff",
  },

  tab_bar = {
    visible = true,       -- 改成 false = 极简模式:整条栏藏起来,
                          --   终端占满全高(快捷键照常工作)
    compact = true,       -- true = 紧凑标签栏;false = 更宽松的栏高
    -- 颜色全都可以不写:不写就从主题自动搭配,换主题自动协调
  },
}
```

不知道颜色怎么配?打开设置面板(`Ctrl+,`,或点标签栏的齿轮),
设置以独立浮层显示，不改变终端行数。外观、背景、快捷键分为三个页面；主题可直接预览，透明度、图片强度和亮度可点击或拖动调整，图片可用桌面文件选择器选择，也可编辑路径。「撤销上次修改」撤销上一次外观操作，「恢复默认外观」回到出厂外观。点击字段后输入，Enter 应用、Esc 取消、Ctrl+A 替换、Ctrl+V 粘贴，改完自动
保存到配置文件(你手写的注释和逻辑都会保留)。

改字号还有更快的:`Ctrl+=` 放大、`Ctrl+-` 缩小、`Ctrl+0` 复原,
或在设置面板点击字号字段。

想换全局呼出键(默认 F12):

```lua
global_shortcut = "CTRL+grave",  -- 改完重启 tayd,按系统弹窗确认
```

更多现成的例子看 [config/desktop.lua](config/desktop.lua)
(冰蓝配色 + 12pt 字体 + 全套快捷键)。

## 除了当终端,它还会这些

- **每条命令一个"块"**:命令跑完,行尾会挂一个红色小徽章标
  出错;`Ctrl+Shift+O` 一键复制上一条命令的输出(只有输出,
  不含提示符);点徽章或 `Ctrl+Shift+H` 可以把输出折叠起来;
  `Ctrl+Shift+↑/↓` 在提示符之间跳。bash/zsh/fish 需要装一个
  几行的钩子(见 [docs/shell-integration.md](docs/shell-integration.md))。
- **QuickSelect**(`Ctrl+Shift+F`):屏幕上的网址/路径/git 哈希
  会被打上标签,按一下字母键就复制,Shift+字母顺便用浏览器打开。
- **vi 复制模式**(`Ctrl+Shift+Space`):hjkl 移动、`/` 搜索、
  `y` 复制,不碰鼠标。
- **会话恢复**:`tay quit` 会记下开了哪些标签,`tay resurrect`
  全部拉回来(滚动历史不会恢复)。
- **图片**:kitty / sixel / iTerm2 三种终端图片协议都支持。
- **链接**:输出里的 URL 有下划线,`Ctrl+点击` 直接用浏览器打开。
- **可视铃**:程序响铃时屏幕会轻微闪一下,替代刺耳的哔声。

## 给开发者

代码结构(想看更多细节读 [docs/architecture.md](docs/architecture.md)):

```
crates/
├── tay/         命令行小工具(通过 Unix socket 指挥 daemon)
├── tayd/        常驻进程:终端内核 + 自研事件循环 + 呼出状态机
├── tay-window/  Wayland 窗口层(手写 layer-shell)
├── tay-render/  GPU 渲染(wgpu + glyphon)
└── tay-config/  Lua 配置解析(沙箱求值)
```

质量检查:

```bash
cargo test --workspace           # 单元测试 + 属性测试
cargo clippy --workspace --all-targets -- -D warnings
```

还有几把自动验证的"尺子":`TAY_SHOT=路径,帧数` 自动截图退出、
`TAY_EXEC=命令` 启动后自动执行、`TAY_START_VISIBLE=1` 启动即呼出
——不用真机手动点也能验收。`python3 tools/usability-smoke.py`
跑一遍端到端冒烟(用独立配置和 socket,不碰你正在用的实例)。

许可证:**GPL-3.0**。

### 背景图片与模糊

在现有 `~/.config/tay/config.lua` 的返回表中加入以下字段，保存后自动重载：

```lua
background_image = "/home/用户名/Pictures/wallpaper.jpg", -- PNG/JPEG；空字符串禁用
background_image_opacity = 0.25, -- 图片自身不透明度，0～1
background_image_brightness = 0.45, -- 图片亮度，0～1；与窗口透明度独立
background_blur = true, -- KDE/KWin 背景模糊，需桌面启用模糊效果
opacity = 0.85, -- 终端纯色背景不透明度
```

图片居中覆盖窗口并随窗口尺寸缩放；支持绝对路径和 `~/`。
为显示模糊效果应保留一定透明度。桌面不支持 KDE 模糊协议时自动降级；
图片读取或解码失败时使用纯色背景，原因会显示在设置面板的背景页，
修复文件后自动恢复。

### 关闭保护与运行诊断

默认 `confirm_close = true`。检测到终端前台任务时，关闭标签、窗格或整个 Tay
会显示确认面板：Ctrl+Enter 或点击 Close 确认，Enter / Esc 或点击 Cancel 取消。
空闲 Shell 直接关闭。需要明确跳过保护时使用 `tay quit --force`；
脚本也可以使用 `tay confirm-close` / `tay cancel-close`。
注意：0.1.0-34 之前 `tay quit` 无条件退出；从本版起有前台任务时改为显示确认，
依赖旧行为的脚本或快捷键请改用 `tay quit --force`。
保护基于 Linux 控制终端的前台进程组，不是未保存文件或所有后台任务的检测器。

`tay status` 查询运行中的 daemon，显示实际快捷键绑定反馈、窗口及 GPU 阶段、
模糊请求与应用状态、配置路径、会话数量和最近反馈；不会启动新 daemon。
`tay doctor` 同时显示环境能力与运行状态。Portal 接口可用不等于快捷键已获授权。
托盘显示实际快捷键、窗口状态与标签数量。安装升级后需保存工作并重启 daemon。

### 平台支持边界

Tay 当前要求 Wayland、wlr-layer-shell，以及可用的 GPU 渲染后端。
隔离 KWin 桌面已覆盖呼出、隐藏、焦点、Portal 与托盘模拟服务和布局恢复；
其他桌面需要分别验证 layer-shell、全局快捷键和托盘能力。
不提供 layer-shell 的桌面当前无法启动 Tay，没有普通窗口回退。
模糊是可选增强，依赖桌面提供 KDE 模糊协议；物理快捷键、输入法、多屏及分数缩放
仍应在目标桌面人工验收，自动化通过不代表这些环境全部通过。

设置面板支持 Tab / Shift+Tab 或方向键导航、Enter 操作、Esc 关闭；
`tay settings appearance`、`tay settings background`、`tay settings shortcuts` 可直达页面。
文件选择需要桌面 FileChooser Portal；不可用时仍可编辑本地图片路径。
浮层沿用当前文字渲染与字号体系，尚未拆分独立的界面字体大小。
