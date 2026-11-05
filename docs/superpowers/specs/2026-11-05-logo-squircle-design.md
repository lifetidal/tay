# Tay Logo 边框改为 One UI Squircle(设计记录)

日期:2026-11-05
状态:已实施(方案 A,用户确认)

## 意图

用户希望项目 Logo 的边框形状改为「本地 Kate 编辑器的形状」。
核实本机环境:系统图标主题为 OneUI-dark,本地 Kate 图标显示为
One UI squircle 边框 + 蓝青渐变(而非官方圆形徽章);
`/usr/share/applications` 下 .desktop 文件图标同为此形状。
用户确认采用方案 A:仅替换边框形状。

## 备选方案

- 官方 Kate 圆徽章(圆环边框 / 蓝底满徽章):用户否定(「都不是」)。
- B:OneUI 满底徽章(蓝青渐变底 + 图案内缩):未选。
- A:仅把外形裁切成 One UI squircle:选中。图案、配色不变,squircle 外透明。

## 实现

- squircle 路径取自本机 `~/.local/share/icons/OneUI/scalable/apps/kate.svg`
  的 rect863(64 单位空间,62/64 满幅):
  `M 32 2 C 10.4 2, 2 10.4, 2 32 C 2 53.6, 10.4 62, 32 62 C 53.6 62, 62 53.6, 62 32 C 62 10.4, 53.6 2, 32 2 z`
- 等比缩放至 1254 画布满幅;4 倍超采样(5016px)绘制 mask →
  CopyOpacity 裁切 → Lanczos 缩回 1254px,沿用仓库圆角版的抗锯齿做法。
- 工具:ImageMagick(path 绘制 + 通道合成)。

## 涉及文件

- `packaging/icons/tay.png`:正式 Logo,内容替换。
- `packaging/icons/tay-owl-squircle-v1.png`:新增,图标名版本文件。
- `packaging/tay.desktop`:`Icon=tay-owl-squircle-v1`。
- `crates/tayd/src/tray.rs`:`icon_name()` 同名。
- `packaging/arch/PKGBUILD`:pkgrel 50 → 51,pixmaps 安装名更新。
- `packaging/aur/PKGBUILD`:pixmaps 安装名更新。
- `packaging/org.tay.Tay.json`:flatpak 内 sed 替换的图标名更新。
- `tools/desktop-services-smoke.py`:托盘 IconName 断言更新。
- `.gitignore`:png 白名单新增。

## 验证

- 像素采样:四角透明、squircle 内图案不透明;与 1× 草稿 RMSE 2.1%(仅抗锯齿差异)。
- tayd 托盘单测 `embedded_icon_has_transparent_corners_and_network_argb_pixels`
  通过(64px 缩放下左上角像素仍全透明)。
