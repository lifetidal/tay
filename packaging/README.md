# Tay 打包说明

## Arch Linux：本地 pacman 包

在仓库根目录以普通用户运行：

```bash
sudo pacman -S --needed base-devel rust
./tools/package-arch.sh
sudo pacman -U ./dist/arch/tay-0.1.0-23-$(uname -m).pkg.tar.zst
```

使用 rustup 管理 Rust 时，可保留现有工具链。打包需要支持 Rust 2024 edition 的工具链。
脚本默认运行整个工作区测试，不安装软件、不启动终端、不修改用户配置。
源码来自当前工作目录，包含未提交修改；不会从占位 GitHub 地址下载项目。

可选参数：`--offline` 仅使用 Cargo 缓存；`--syncdeps` 允许 makepkg
通过 pacman 补齐依赖；`--skip-tests` 跳过测试。产物在 `dist/arch/`，
独立构建缓存保存在 `target/arch-package/`。默认拒绝覆盖同版本包；重新发布请递增 pkgrel，明确需要覆盖时使用 `--force`。
底层配方为 `packaging/arch/PKGBUILD`，由脚本提供源码归档、版本和 SHA256。

安装后，从应用菜单打开 Tay，或运行：

```bash
tay show
```

Wayland 桌面必须支持 layer-shell（例如 KDE Plasma、Sway、Hyprland）。
渲染需要可用的 Vulkan 或 OpenGL 驱动。全局快捷键需要桌面的
GlobalShortcuts portal 后端及授权，也可在桌面快捷键设置中绑定 `tay toggle`。

配置示例安装到 `/usr/share/doc/tay/examples/config.lua`。
需要采用该预设时，仅在尚无用户配置时复制：

```bash
mkdir -p ~/.config/tay
cp -n /usr/share/doc/tay/examples/config.lua ~/.config/tay/config.lua
```

已有 Lua 配置保持不变；软件包不向用户家目录写文件。
0.1.0-2 会为已有 keys 表继承未覆盖的默认键，因此旧配置也能使用 Ctrl+T。
如需 WezTerm 参考配色，请在现有 config.lua 中设置 `theme = "wezterm"`，
并在 font 表中加 `weight = 500`。0.1.0-4 起统一使用 Lua，旧 appearance.toml
会一次性迁入 config.lua 后归档为 .migrated.bak；已有 Lua 逻辑和注释保留。
升级后正在运行的旧 daemon 仍使用旧程序；退出其会话并重新打开 Tay 后才使用新版。
如需登录时启动，可在图形会话中执行：

```bash
systemctl --user enable --now tayd.service
```

服务不会自动启用。若之前使用 `tay show` 启动了独立 daemon，先执行
`tay quit` 再启动服务，避免重复实例。默认是否显示窗口由用户配置的
`start_hidden` 决定。卸载：`sudo pacman -R tay`。

`packaging/aur/PKGBUILD` 是从项目 Git 仓库构建的开发版配方，
不用于上述本地打包流程。

## 其他发行版

- 二进制 + systemd 单元 + desktop 文件都在本目录,按各发行版
  规范放置即可(`/usr/bin`、`/usr/lib/systemd/user/`、
  `/usr/share/applications/`)。
- Flatpak:manifest 见 org.tay.Tay.json(portal 权限已声明:
  Desktop + GlobalShortcuts)。注意:沙箱内 $XDG_RUNTIME_DIR 与宿主机
  不同,宿主机的 `tay` CLI 无法直接连沙箱 daemon 的 IPC socket ——
  沙箱场景请用沙箱内的 `tay`(或 portal 热键)。全链路验证见
  DESIGN §9 M4 验收。

0.1.0-22 起使用固定 Portal 应用身份 `org.tay.Tay`（菜单入口
`org.tay.Tay.desktop`），不再依赖启动它的终端身份。首次运行可能需要
重新确认全局快捷键授权。实际快捷键由桌面最终决定，托盘提示及菜单显示
授权结果；未分配时可用托盘呼出，或在桌面绑定 `tay toggle`。
托盘左键切换窗口，右键提供显示、隐藏、设置与退出；隐藏保留托盘和会话。
