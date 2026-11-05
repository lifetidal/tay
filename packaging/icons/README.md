# Tay 项目 Logo

当前正式 Logo 是 `tay.png`,2026-11-05 起为「One UI squircle 边框版」:
2026-10-21 确认的 v5 图案与配色不变,外形由 23% 圆角改为 One UI squircle,
squircle 之外区域透明。经用户确认采用方案 A(仅改边框)。

- 原始选定稿:`tay-owl-logo-v5.png`。
- 形象:猫头鹰从树丛中探出,伸出一只爪子,带叶细枝轻遮额头。
- 配色:深蓝轮廓、绿色枝叶、橙色眼睛,浅色实底。
- 生成方式与提示词:见 [OWL-LOGO-v5-PROMPT.md](OWL-LOGO-v5-PROMPT.md)。
- squircle 边框来源与裁切实现:见
  [../../docs/superpowers/specs/2026-11-05-logo-squircle-design.md](../../docs/superpowers/specs/2026-11-05-logo-squircle-design.md)。
- 原雪鸮破冰图标备份:`tay-owl-ice-original.png`;更早的终端图形保存在 `tay-terminal.png`。

README 展示和 Arch/AUR 打包均引用 `packaging/icons/tay.png`,后续打包会自动使用本版。其他带版本名的图片是设计过程稿。

Arch/AUR 菜单入口使用 `Icon=tay-owl-squircle-v1`,对应
`/usr/share/pixmaps/tay-owl-squircle-v1.png`;托盘 IconName 同名。
保留 `tay.png` 作为兼容路径,避免复用旧图标名称后菜单仍显示旧图。

历史:2026-10-21 版本为 `tay-owl-rounded-v1.png`(23% 半径圆角);
2026-11-05 起替换为 `tay-owl-squircle-v1.png`(One UI squircle 边框)。
