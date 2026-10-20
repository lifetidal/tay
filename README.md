# Tay

**按一下按键,终端就从屏幕顶部滑下来;再按一下,它就藏回去。**

一个 Linux 下拉终端(像老游戏里的 Quake 控制台)。Rust 写的,跑在
Wayland 上,没装任何配置也能直接干活。呼出只要 1~16 毫秒——因为
它平时就待在后台等着你。

设计蓝图见 [DESIGN.md](DESIGN.md);开发过程记录见 [docs/](docs/)。

## 装

**Arch Linux**(打包脚本会用当前源码打出 .pkg.tar.zst):

```bash
./tools/package-arch.sh --offline
sudo pacman -U ./dist/arch/tay-*.pkg.tar.zst
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

## 让它好看

所有外观都在一个文件里:`~/.config/tay/config.lua`。
改完保存,**立即生效**,不用重启。写坏了也没关系——它会
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
    compact = true,       -- true = 只有一行标签;false = 三行完整栏
    -- 颜色全都可以不写:不写就从主题自动搭配,换主题自动协调
  },
}
```

不知道颜色怎么配?打开设置面板(`Ctrl+,`,或点标签栏的齿轮),
**Font / Background / Text color** 可以直接点着改,改完自动
保存到配置文件(你手写的注释和逻辑都会保留)。

改字号还有更快的:`Ctrl+=` 放大、`Ctrl+-` 缩小、`Ctrl+0` 复原,
或点标签栏的 `A+` / `A-`。

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
