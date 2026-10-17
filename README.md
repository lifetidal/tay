# Tay

**Linux 一键呼出下拉终端** —— Rust 原生、Wayland layer-shell、零配置可用。

设计蓝图见 [DESIGN.md](DESIGN.md);实施笔记见 [docs/](docs/)。

## 特性

- **热呼出 < 50ms**(实测 16ms 首次 / 1ms 常驻):daemon 常驻 + layer
  surface 全程 mapped,呼出只动 top margin,GPU 资源零重建
- **一键呼出**:GlobalShortcuts portal(KDE/Hyprland)或 `tay toggle`
- **失焦自动隐藏**、150ms ease-out 滑入动画、每输出独立呼出
- **OSC 133 命令块**(fish 4+ 零配置):退出码徽章、折叠(点徽章 /
  Ctrl+Shift+H)、提示符跳转(Ctrl+Shift+↑/↓)、按块复制(Ctrl+Shift+O)
- **QuickSelect**(Ctrl+Shift+F):URL/路径/git hash 打标签,按键即复制
  (URL 同时 xdg-open)
- **vi 复制模式**(Ctrl+Shift+Space):hjkl 移动、`/` 全滚动回退搜索、
  `y` 复制
- **图像协议三件套**:kitty graphics / sixel / iTerm2 inline(PNG/RGBA/RGB)
- **主题热重载**:改 `~/.config/tay/config.toml` 即时生效
- CJK 与 IME(text-input-v3)、ANSI 真色、滚动性能 >1000fps

## 安装

```bash
# Arch(AUR,仓库公开后)
yay -S tay-git

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
```

首次按 **F12** 会在 KDE 弹窗确认一次全局热键绑定。

## 配置

`~/.config/tay/config.toml`(全部可选,热重载):

```toml
[font]
family = "JetBrainsMono Nerd Font"
size = 12.0

[colors]
foreground = "#d4d4d4"
background = "#0f1114"
cursor = "#00a884"

shell = ""            # 空 = $SHELL
height_percent = 45   # 呼出高度占输出百分比
animation_ms = 150    # 滑入动画,0 = 关闭
hide_on_focus_out = true
start_hidden = true
output = ""           # 呼出输出名(如 "DP-1");空 = 合成器默认
```

## 开发

验证环境变量:`TAY_SHOT=路径[,帧数]`(截图退出)、`TAY_BENCH=帧数`
(滚动基准)、`TAY_EXEC=命令`(启动后执行)、`TAY_START_VISIBLE=1`、
`TAY_SHELL=路径`。

```bash
cargo test                 # 27 个单元测试
cargo run -p tayd          # 交互体验
```

## 架构

```
crates/
├── tay/             CLI 薄客户端(Unix socket IPC)
├── tayd/            常驻 daemon:终端内核(alacritty_terminal)+
│                    自研 PTY 事件循环(OSC 133/图像协议流拦截)+
│                    portal 热键 + 呼出状态机
├── tay-window/      手写 SCTK + wlr-layer-shell 窗口层
├── tay-render/      wgpu + glyphon 渲染管线(网格/徽章/overlay/图像)
└── tay-config/      TOML 配置
```

许可证:**GPL-3.0**(终端内核 alacritty_terminal 为 Apache-2.0)。
