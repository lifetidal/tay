//! Settings overlay: one layout drives painting, pointer hits and keyboard focus.
use crate::chrome::{Action, Field, Item};
use tay_config::{Config, Rgb};
use tay_render::{Cell, Grid};
use unicode_width::UnicodeWidthChar;
pub(crate) fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    Rgb::new(
        (a.0 as f32 * (1. - t) + b.0 as f32 * t) as u8,
        (a.1 as f32 * (1. - t) + b.1 as f32 * t) as u8,
        (a.2 as f32 * (1. - t) + b.2 as f32 * t) as u8,
    )
}
pub(crate) fn field_label(field: &Field) -> &'static str {
    match field {
        Field::Font => "字体",
        Field::Size => "字号",
        Field::Opacity => "窗口不透明度",
        Field::Wallpaper => "图片路径",
        Field::ImageOpacity => "图片强度",
        Field::ImageBrightness => "图片亮度",
        Field::Foreground => "文字颜色",
        Field::Background => "背景颜色",
        Field::Theme => "主题",
    }
}
pub(crate) fn rect(cols: usize, rows: usize) -> (usize, usize, usize, usize) {
    let w = cols.saturating_sub(4).min(88).max(cols.min(4));
    let h = rows.saturating_sub(2).min(16).max(rows.min(2));
    ((cols - w) / 2, (rows - h) / 2, w, h)
}
pub(crate) fn layout(cols: usize, rows: usize, page: u8, c: &Config) -> Vec<Item> {
    let (x, y, w, h) = rect(cols, rows);
    if w < 4 || h < 4 {
        return vec![Item {
            row: y,
            start: x,
            end: x + w,
            text: "Close".into(),
            action: Action::Settings,
            selected: false,
        }];
    }
    let mut items = vec![];
    let mut add =
        |row: usize, start: usize, end: usize, text: String, action: Action, selected: bool| {
            if row < y + h - 1 && start < end && end <= x + w {
                items.push(Item {
                    row,
                    start,
                    end,
                    text,
                    action,
                    selected,
                });
            }
        };
    add(
        y,
        x + w - 8.min(w),
        x + w - 1,
        " 关闭 ".into(),
        Action::Settings,
        false,
    );
    let nav = (w - 2) / 3;
    for (i, label) in ["外观", "背景", "快捷键"].iter().enumerate() {
        add(
            y + 2,
            x + 1 + i * nav,
            x + 1 + (i + 1) * nav,
            format!(" {label} "),
            Action::SettingsPage(i as u8),
            page == i as u8,
        );
    }
    let left = x + 2;
    let right = x + w - 2;
    let mid = left + (right - left) / 2;
    let base = if h < 13 { 3 } else { 4 };
    let step = if h < 13 { 1 } else { 2 };
    match page {
        0 => {
            let tile = (right - left) / 4;
            for (i, name) in ["default", "midnight", "light", "wezterm"]
                .iter()
                .enumerate()
            {
                add(
                    y + base,
                    left + i * tile,
                    left + (i + 1) * tile,
                    format!(" {name} "),
                    Action::Theme(i as u8),
                    c.theme == *name,
                );
            }
            add(
                y + base + step,
                left,
                mid,
                format!(" 字体: {}", c.font.family),
                Action::Edit(Field::Font),
                false,
            );
            add(
                y + base + step,
                mid,
                right,
                format!(" 字号: {} pt", c.font.size),
                Action::Edit(Field::Size),
                false,
            );
            add(
                y + base + 2 * step,
                left,
                mid,
                " 文字颜色  >".into(),
                Action::Edit(Field::Foreground),
                false,
            );
            add(
                y + base + 2 * step,
                mid,
                right,
                " 背景颜色  >".into(),
                Action::Edit(Field::Background),
                false,
            );
            add(
                y + base + 3 * step,
                left,
                mid,
                " 撤销上次修改 ".into(),
                Action::UndoAppearance,
                false,
            );
            add(
                y + base + 3 * step,
                mid,
                right,
                " 恢复默认外观 ".into(),
                Action::ResetAppearance,
                false,
            );
        }
        1 => {
            add(
                y + base,
                left,
                mid,
                " 选择图片... ".into(),
                Action::PickWallpaper,
                false,
            );
            add(
                y + base,
                mid,
                right,
                " 编辑图片路径 > ".into(),
                Action::Edit(Field::Wallpaper),
                false,
            );
            for (offset, label, value, field) in [
                (1, "窗口不透明度", c.opacity, Field::Opacity),
                (
                    2,
                    "图片强度",
                    c.background_image_opacity,
                    Field::ImageOpacity,
                ),
                (
                    3,
                    "图片亮度",
                    c.background_image_brightness,
                    Field::ImageBrightness,
                ),
            ] {
                let row = y + base + offset * step;
                let segment = (right.saturating_sub(left + 24)) / 11;
                if segment == 0 {
                    add(
                        row,
                        left,
                        right,
                        format!(" {label}: {:.0}% >", value * 100.),
                        Action::Edit(field),
                        false,
                    );
                } else {
                    add(
                        row,
                        left,
                        left + 24,
                        format!(" {label}: {:.0}%", value * 100.),
                        Action::Edit(field),
                        false,
                    );
                    for i in 0..=10 {
                        let selected = (value * 10.).round() as u8 == i;
                        add(
                            row,
                            left + 24 + i as usize * segment,
                            left + 24 + (i as usize + 1) * segment,
                            if selected {
                                format!("●{}", " ".repeat(segment.saturating_sub(1)))
                            } else {
                                "━".repeat(segment)
                            },
                            Action::Level(field, i * 10),
                            selected,
                        );
                    }
                }
            }
            add(
                y + base + 4 * step,
                left,
                mid,
                format!(" 模糊  [{}]", if c.background_blur { "开" } else { "关" }),
                Action::Blur,
                c.background_blur,
            );
            add(
                y + base + 4 * step,
                mid,
                right,
                " 移除图片 ".into(),
                Action::ClearWallpaper,
                false,
            );
        }
        _ => {
            add(
                y + base + step,
                left,
                right,
                " 配置桌面快捷键 > ".into(),
                Action::Hotkeys,
                false,
            );
            add(
                y + 9,
                left,
                right,
                " 复制诊断报告 ".into(),
                Action::Diagnostics,
                false,
            );
        }
    }
    items
}
#[allow(clippy::too_many_arguments)]
pub(crate) fn put(
    grid: &mut Grid,
    row: usize,
    start: usize,
    end: usize,
    text: &str,
    fg: Rgb,
    bg: Rgb,
    bold: bool,
) {
    if row >= grid.rows {
        return;
    }
    for col in start..end.min(grid.cols) {
        grid.cells[row * grid.cols + col] = Cell {
            c: ' ',
            fg,
            bg,
            bold,
            italic: false,
            underline: false,
            wide_spacer: false,
            zerowidth: None,
        };
    }
    let mut col = start;
    for c in text.chars().filter(|c| !c.is_control()) {
        let width = c.width().unwrap_or(0);
        if width == 0 {
            continue;
        }
        if col + width > end.min(grid.cols) {
            break;
        }
        grid.cells[row * grid.cols + col].c = c;
        for offset in 1..width {
            grid.cells[row * grid.cols + col + offset].wide_spacer = true;
        }
        col += width;
    }
}
/// 面板绘制所需的只读上下文(打包避免 paint 参数过载)。
pub(crate) struct OverlayInfo<'a> {
    pub(crate) status: &'a str,
    pub(crate) feedback: &'a str,
    pub(crate) background_error: Option<&'a str>,
    pub(crate) undo_available: bool,
}
pub(crate) fn paint(
    grid: &mut Grid,
    c: &Config,
    page: u8,
    focus: Option<Action>,
    editing: Option<&(Field, String)>,
    info: &OverlayInfo<'_>,
) {
    let (x, y, w, h) = rect(grid.cols, grid.rows);
    let fg = c.colors.foreground;
    let backdrop = c.tab_bar.background;
    let bg = mix(backdrop, fg, 0.06);
    let muted = mix(fg, bg, 0.35);
    let raised = mix(bg, fg, 0.10);
    let accent = c.tab_bar.accent;
    for cell in &mut grid.cells {
        cell.bg = mix(cell.bg, backdrop, 0.65);
        cell.fg = mix(cell.fg, bg, 0.65);
    }
    let surround = grid
        .cells
        .get(y * grid.cols + x)
        .map(|cell| cell.bg)
        .unwrap_or(bg);
    grid.chrome_panels.push(tay_render::ChromePanel {
        row: y,
        rows: h,
        start: x,
        end: x + w,
        background: surround,
        accent: None,
        corners: tay_render::PanelCorners::All,
    });
    for row in y..y + h {
        put(grid, row, x, x + w, "", fg, bg, false);
    }
    put(
        grid,
        y,
        x + 1,
        (x + w).saturating_sub(8),
        " 设置",
        fg,
        bg,
        true,
    );
    for item in layout(grid.cols, grid.rows, page, c) {
        let unavailable = matches!(item.action, Action::UndoAppearance) && !info.undo_available;
        let active = item.selected || (focus == Some(item.action) && !unavailable);
        let color = if unavailable {
            mix(raised, bg, 0.5)
        } else if active {
            mix(bg, accent, 0.35)
        } else {
            raised
        };
        let (button_fg, button_bg) = if let Action::Theme(index) = item.action {
            let palette = tay_config::Colors::preset(
                ["default", "midnight", "light", "wezterm"][index as usize],
            )
            .unwrap_or_else(|| c.colors.clone());
            (
                palette.foreground,
                if active {
                    mix(palette.background, accent, 0.3)
                } else {
                    palette.background
                },
            )
        } else if unavailable {
            (muted, color)
        } else {
            (fg, color)
        };
        put(
            grid, item.row, item.start, item.end, &item.text, button_fg, button_bg, active,
        );
    }
    if h > 6 && page == 2 {
        let shortcut = info
            .status
            .split('\t')
            .find_map(|s| s.strip_prefix("shortcut="))
            .unwrap_or("快捷键绑定待确认");
        put(grid, y + 4, x + 2, x + w - 2, shortcut, muted, bg, false);
        put(
            grid,
            y + 8,
            x + 2,
            x + w - 2,
            "高级诊断:tay status / tay doctor",
            muted,
            bg,
            false,
        );
    }
    if h > 13 && page == 0 {
        put(
            grid,
            y + 12,
            x + 2,
            x + w - 2,
            "Preview  ~/project  $ cargo test",
            c.colors.foreground,
            c.colors.background,
            false,
        );
        put(
            grid,
            y + 13,
            x + 2,
            x + w - 2,
            "PASS  0123456789  AaBb  中文",
            c.colors.ansi[2],
            c.colors.background,
            false,
        );
    }
    let footer = if let Some((field, value)) = editing {
        format!("{}: {value}_  [Enter 应用 / Esc 取消]", field_label(field))
    } else if page == 1
        && !c.background_image.is_empty()
        && let Some(error) = info.background_error
    {
        format!("图片加载失败:{error}")
    } else if page == 1 && !c.background_image.is_empty() && c.opacity >= 1.0 {
        // 首次选图最常见的困惑:opacity 默认 1.0,图片被纯色背景完全遮挡。
        "窗口不透明度 100%:图片会被完全遮挡,请调低「窗口不透明度」".into()
    } else if !info.feedback.is_empty() {
        info.feedback.into()
    } else {
        "已自动保存到 Lua · Tab / 方向键导航 · Esc 关闭".into()
    };
    if h > 1 {
        put(grid, y + h - 1, x + 1, x + w - 1, &footer, muted, bg, false);
    }
    grid.opaque_background = true;
    grid.cursor_shape = tay_render::CursorShape::Hidden;
    grid.preedit = None;
    grid.selection = None;
    grid.overlays.clear();
    grid.images.clear();
    grid.window_icons.clear();
}
pub(crate) fn drag_level(items: &[Item], field: Field, col: f64) -> Option<Action> {
    items
        .iter()
        .filter(|item| matches!(item.action,Action::Level(f,_) if f==field))
        .min_by(|a, b| {
            let distance = |item: &Item| (((item.start + item.end) as f64 / 2.) - col).abs();
            distance(a).total_cmp(&distance(b))
        })
        .map(|item| item.action)
}
pub(crate) fn local_file_uri(uri: &str) -> Option<String> {
    let path = uri
        .strip_prefix("file://localhost/")
        .map(|p| format!("/{p}"))
        .or_else(|| {
            uri.strip_prefix("file://")
                .filter(|p| p.starts_with('/'))
                .map(str::to_owned)
        })?;
    let path = percent_encoding::percent_decode_str(&path)
        .decode_utf8()
        .ok()?
        .into_owned();
    (!path.contains('\0')).then_some(path)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn slider_drag_clamps_to_its_own_track() {
        let items = layout(128, 18, 1, &Config::default());
        assert_eq!(
            drag_level(&items, Field::Opacity, -100.),
            Some(Action::Level(Field::Opacity, 0))
        );
        assert_eq!(
            drag_level(&items, Field::Opacity, 10000.),
            Some(Action::Level(Field::Opacity, 100))
        );
        assert_eq!(drag_level(&items, Field::Font, 10.), None);
    }
    #[test]
    fn picker_accepts_local_escaped_paths_and_rejects_remote_uris() {
        assert_eq!(
            local_file_uri("file:///tmp/a%20b.png").as_deref(),
            Some("/tmp/a b.png")
        );
        assert_eq!(
            local_file_uri("file://localhost/tmp/a.png").as_deref(),
            Some("/tmp/a.png")
        );
        assert!(local_file_uri("file://remote/tmp/a.png").is_none());
        assert!(local_file_uri("https://example.com/a.png").is_none());
        assert!(local_file_uri("file:///tmp/%00.png").is_none());
    }
    #[test]
    fn overlay_actions_stay_inside_bounds_and_do_not_resize_terminal() {
        for (cols, rows) in [(128, 18), (60, 16), (24, 12), (2, 1)] {
            for page in 0..3 {
                let items = layout(cols, rows, page, &Config::default());
                for item in items {
                    assert!(item.row < rows && item.end <= cols && item.start < item.end);
                }
            }
        }
        let c = Config::default();
        assert_eq!(
            crate::chrome::rows(&c, true),
            crate::chrome::rows(&c, false)
        );
        let items = layout(128, 18, 0, &c);
        assert_eq!(
            items
                .iter()
                .filter(|i| matches!(i.action, Action::Theme(_)))
                .count(),
            4
        );
    }
}
