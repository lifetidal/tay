# Tay 打包说明

## Arch Linux(AUR)

```bash
cd packaging/aur
makepkg -si        # 或提交到 AUR 后: yay -S tay-git
```

安装后:

```bash
# 随登录自启(图形会话内)
systemctl --user enable --now tayd.service

# 或手动
tayd &            # daemon 常驻,窗口隐藏
tay toggle        # 呼出/收起
```

## 其他发行版

- 二进制 + systemd 单元 + desktop 文件都在本目录,按各发行版
  规范放置即可(`/usr/bin`、`/usr/lib/systemd/user/`、
  `/usr/share/applications/`)。
- Flatpak:manifest 见 org.tay.Tay.json(portal 权限已声明:
  Desktop + GlobalShortcuts)。注意:沙箱内 $XDG_RUNTIME_DIR 与宿主机
  不同,宿主机的 `tay` CLI 无法直接连沙箱 daemon 的 IPC socket ——
  沙箱场景请用沙箱内的 `tay`(或 portal 热键)。全链路验证见
  DESIGN §9 M4 验收。
