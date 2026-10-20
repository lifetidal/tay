//! 可见操作栏。布局与点击共享同一组范围，避免画面与命中不一致。
//!
//! 美化原则(2026-10-19):颜色全部由终端配色(config.lua 的
//! colors 表)推导 —— 底色 = 背景轻微提亮,文字 = 前景,选中标签 =
//! 光标色混底色 + 加粗 + 下划线,关闭 × = 暖红,次要行(控件/设置)
//! = 前景 45% 混底色。列宽一律按**字符数**计(× 等多字节字符
//! 不破坏布局与命中)。
use tay_config::{Config, Rgb};
use tay_render::{Cell, Grid};
use unicode_width::UnicodeWidthChar;

pub const ROWS: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Font,
    Background,
    Foreground,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Tab(usize),
    Close(usize),
    New,
    Settings,
    Hotkeys,
    Smaller,
    Larger,
    Hide,
    Maximize,
    CloseWindow,
    Edit(Field),
}
#[derive(Clone)]
pub struct Item {
    pub row: usize,
    /// 列范围(字符列,含起点、不含终点)。
    pub start: usize,
    pub end: usize,
    pub text: String,
    pub action: Action,
    pub selected: bool,
}

/// 通道线性插值(0..1,钳制)。
fn lerp(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t)
        .round()
        .clamp(0.0, 255.0) as u8
}

/// 颜色混合:a 混入 t 比例的 b。
fn blend(a: Rgb, b: Rgb, t: f32) -> Rgb {
    Rgb::new(lerp(a.0, b.0, t), lerp(a.1, b.1, t), lerp(a.2, b.2, t))
}

/// 布局(纯函数):标签页一行 + 控件一行 + 设置一行(开启时)。
/// 列宽按字符数计,start/end 与 text 的字符数严格一致。
pub fn layout(
    cols: usize,
    titles: &[String],
    active: usize,
    settings: bool,
    config: &Config,
) -> Vec<Item> {
    let mut items = Vec::new();
    let mut x = 0usize;

    // —— 行 0:标签页(分页;活动标签始终在本页)——
    let control_width = if cols >= 40 { 4 } else { 3 };
    let reserved = control_width * 3;
    let page = (cols.saturating_sub(reserved) / 26).max(1);
    let first = (active / page) * page;
    for (i, title) in titles.iter().enumerate().skip(first).take(page) {
        let name: String = title.chars().filter(char::is_ascii).take(12).collect();
        let label = format!(
            "{}:{}",
            i + 1,
            if name.is_empty() { "shell" } else { &name }
        );
        let mut tab_text = format!(" {label} ");
        let close_text = " × ".to_string();
        let close_w = close_text.chars().count();
        let available = cols.saturating_sub(reserved + x + close_w);
        if available == 0 {
            break;
        }
        tab_text = tab_text.chars().take(available).collect();
        let tab_w = tab_text.chars().count();
        items.push(Item {
            row: 0,
            start: x,
            end: x + tab_w,
            text: tab_text,
            action: Action::Tab(i),
            selected: i == active,
        });
        x += tab_w;
        items.push(Item {
            row: 0,
            start: x,
            end: x + close_w,
            text: close_text,
            action: Action::Close(i),
            selected: i == active,
        });
        x += close_w;
    }
    if cols >= reserved {
        for (index, action) in [Action::Hide, Action::Maximize, Action::CloseWindow]
            .into_iter()
            .enumerate()
        {
            let start = cols - reserved + index * control_width;
            items.push(Item {
                row: 0,
                start,
                end: start + control_width,
                text: " ".repeat(control_width),
                action,
                selected: false,
            });
        }
    }

    // —— 行 1:控件 ——
    let controls: Vec<(String, Action)> = if cols < 50 {
        vec![
            ("+".into(), Action::New),
            ("A-".into(), Action::Smaller),
            ("A+".into(), Action::Larger),
            ("Set".into(), Action::Settings),
            ("Keys".into(), Action::Hotkeys),
        ]
    } else {
        vec![
            ("+ Tab".into(), Action::New),
            ("Settings".into(), Action::Settings),
            ("Hotkeys".into(), Action::Hotkeys),
            ("A-".into(), Action::Smaller),
            (format!("A+ {}pt", config.font.size), Action::Larger),
            ("Hide".into(), Action::Hide),
        ]
    };
    x = 0;
    for (label, action) in controls {
        let text = format!(" {label} ");
        let w = text.chars().count();
        if x + w > cols {
            break;
        }
        items.push(Item {
            row: 1,
            start: x,
            end: x + w,
            text,
            action,
            selected: false,
        });
        x += w;
    }

    // —— 行 2:设置(开启时)——
    if settings {
        x = 0;
        for (label, action) in [
            ("Font", Action::Edit(Field::Font)),
            ("Background", Action::Edit(Field::Background)),
            ("Text color", Action::Edit(Field::Foreground)),
        ] {
            let label = if cols < 40 {
                match label {
                    "Background" => "BG",
                    "Text color" => "Text",
                    other => other,
                }
            } else {
                label
            };
            let text = format!(" {label} ");
            let w = text.chars().count();
            if x + w > cols {
                break;
            }
            items.push(Item {
                row: 2,
                start: x,
                end: x + w,
                text,
                action,
                selected: false,
            });
            x += w;
        }
    }
    items
}

pub fn hit(items: &[Item], row: usize, col: usize) -> Option<Action> {
    items
        .iter()
        .find(|i| i.row == row && col >= i.start && col < i.end)
        .map(|i| i.action)
}

/// 把操作栏叠加到渲染网格顶部(前 3 行),终端内容与光标/选区/
/// 预编辑/徽章/图像随之下移。颜色由终端配色推导,见模块文档。
pub fn decorate(
    grid: &mut Grid,
    items: &[Item],
    message: &str,
    config: &Config,
    hovered: Option<Action>,
    maximized: bool,
) {
    let bg = blend(config.colors.background, Rgb::new(0xff, 0xff, 0xff), 0.07);
    let fg = config.colors.foreground;
    let dim = blend(fg, bg, 0.15);
    let accent = if config.theme == "wezterm" {
        config.colors.ansi[8]
    } else {
        blend(config.colors.cursor, bg, 0.32)
    };
    let close = config.colors.ansi[1];
    let button_bg = blend(bg, fg, 0.15);
    let on = |color: Rgb| {
        if color.0 as u32 * 299 + color.1 as u32 * 587 + color.2 as u32 * 114 > 150000 {
            Rgb(18, 22, 28)
        } else {
            Rgb(245, 247, 250)
        }
    };
    let base = Cell {
        c: ' ',
        fg: dim,
        bg,
        bold: false,
        italic: false,
        underline: false,
        wide_spacer: false,
        zerowidth: None,
    };
    let mut header = vec![base.clone(); ROWS * grid.cols];
    for item in items {
        for (i, c) in item.text.chars().enumerate() {
            if i >= item.end - item.start {
                break;
            }
            let cell = &mut header[item.row * grid.cols + item.start + i];
            cell.c = c;
            let button = !matches!(item.action, Action::Tab(_));
            cell.fg = fg;
            cell.bg = if button {
                button_bg
            } else if config.theme == "wezterm" {
                Rgb(49, 50, 68)
            } else {
                bg
            };
            cell.bold = button || item.selected;
            if item.selected {
                cell.bg = accent;
                cell.fg = if config.theme == "wezterm" {
                    fg
                } else {
                    on(accent)
                };
                cell.underline = true;
            }
            if matches!(item.action, Action::Hide) {
                cell.bg = blend(bg, config.colors.cursor, 0.65);
                cell.fg = on(cell.bg);
            }
            if matches!(item.action, Action::Close(_) | Action::CloseWindow) {
                cell.bg = blend(bg, close, 0.15);
                cell.fg = close;
            }
            if hovered == Some(item.action) {
                cell.bg = if matches!(item.action, Action::Close(_) | Action::CloseWindow) {
                    close
                } else {
                    config.colors.cursor
                };
                cell.fg = on(cell.bg);
                cell.underline = true;
            }
        }
    }
    // 帮助/编辑消息:整行弱化显示。
    if !message.is_empty() {
        for c in &mut header[2 * grid.cols..] {
            *c = base.clone();
        }
        let mut col = 0;
        for c in message.chars() {
            let width = c.width().unwrap_or(1).max(1);
            if col + width > grid.cols {
                break;
            }
            header[2 * grid.cols + col].c = c;
            header[2 * grid.cols + col].fg = dim;
            for offset in 1..width {
                header[2 * grid.cols + col + offset].wide_spacer = true;
            }
            col += width;
        }
    }
    for item in items.iter().filter(|i| i.row == 0) {
        use tay_render::WindowIconKind;
        let kind = match item.action {
            Action::Hide => WindowIconKind::Minimize,
            Action::Maximize => {
                if maximized {
                    WindowIconKind::Restore
                } else {
                    WindowIconKind::Maximize
                }
            }
            Action::CloseWindow => WindowIconKind::Close,
            _ => continue,
        };
        grid.window_icons.push(tay_render::WindowIcon {
            row: 0,
            start: item.start,
            end: item.end,
            kind,
        });
    }
    header.append(&mut grid.cells);
    grid.cells = header;
    grid.rows += ROWS;
    grid.cursor.1 += ROWS;
    if let Some(s) = &mut grid.selection {
        s.start.1 += ROWS;
        s.end.1 += ROWS;
    }
    if let Some(p) = &mut grid.preedit {
        p.row += ROWS;
    }
    for b in &mut grid.badges {
        b.row += ROWS;
    }
    for o in &mut grid.overlays {
        o.row += ROWS;
    }
    for i in &mut grid.images {
        i.row += ROWS;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn standard_window_controls_have_separate_hit_targets() {
        for cols in [24, 80, 200] {
            let items = layout(cols, &["shell".into()], 0, false, &Config::default());
            let mut previous = 0;
            for action in [Action::Hide, Action::Maximize, Action::CloseWindow] {
                let item = items
                    .iter()
                    .find(|i| i.row == 0 && i.action == action)
                    .unwrap();
                assert!(item.start >= previous);
                assert_eq!(hit(&items, 0, item.start), Some(action));
                assert_eq!(hit(&items, 0, item.end - 1), Some(action));
                previous = item.end;
            }
            assert_eq!(previous, cols);
        }
    }

    #[test]
    fn large_font_keeps_tabs_and_settings_accessible() {
        let titles = vec!["a very long title".into(); 24];
        for cols in [24, 40, 80] {
            for active in 0..24 {
                let items = layout(cols, &titles, active, true, &Config::default());
                for action in [
                    Action::Tab(active),
                    Action::Close(active),
                    Action::Smaller,
                    Action::Edit(Field::Background),
                    Action::Edit(Field::Foreground),
                ] {
                    assert!(
                        items.iter().any(|i| i.action == action),
                        "cols={cols} active={active} action={action:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn active_tab_remains_visible_with_many_tabs() {
        let titles = vec!["shell".to_string(); 24];
        for active in 0..24 {
            let items = layout(80, &titles, active, false, &Config::default());
            assert!(items.iter().any(|i| i.action == Action::Tab(active)));
        }
    }

    #[test]
    fn header_offsets_terminal_content_and_input_visuals() {
        let cell = Cell {
            c: 'z',
            fg: Rgb(255, 255, 255),
            bg: Rgb(0, 0, 0),
            bold: false,
            italic: false,
            underline: false,
            wide_spacer: false,
            zerowidth: None,
        };
        let mut grid = Grid {
            cols: 2,
            rows: 1,
            cells: vec![cell; 2],
            cursor: (1, 0),
            cursor_shape: tay_render::CursorShape::Beam,
            selection: Some(tay_render::SelectionRange {
                start: (0, 0),
                end: (1, 0),
            }),
            preedit: Some(tay_render::Preedit {
                text: "中".into(),
                col: 0,
                row: 0,
                cursor_bytes: None,
            }),
            badges: vec![],
            overlays: vec![],
            images: vec![],
            window_icons: vec![],
        };
        decorate(&mut grid, &[], "help", &Config::default(), None, false);
        assert_eq!(grid.rows, 4);
        assert_eq!(grid.cells.len(), 8);
        assert_eq!(grid.cells[6].c, 'z');
        assert_eq!(grid.cursor, (1, 3));
        assert_eq!(grid.preedit.unwrap().row, 3);
        assert_eq!(grid.selection.unwrap().start, (0, 3));
    }

    #[test]
    fn every_visible_control_hits_its_action() {
        for cols in [12, 40, 80, 160] {
            let items = layout(
                cols,
                &["shell".into(), "logs".into()],
                1,
                true,
                &Config::default(),
            );
            for i in &items {
                assert!(i.end <= cols, "项不越界");
                for col in i.start..i.end {
                    assert_eq!(hit(&items, i.row, col), Some(i.action));
                }
            }
        }
    }

    #[test]
    fn tab_close_is_separate_from_switch() {
        let items = layout(120, &["shell".into()], 0, false, &Config::default());
        assert!(items.iter().any(|i| i.action == Action::Close(0)));
        assert!(items.iter().any(|i| i.action == Action::New));
    }

    /// 多字节字符(×)的列宽按字符数计:布局与渲染严格一致
    /// (旧实现按字节数计,× 会让后续列偏移 1 格)。右对齐的
    /// [Hide ×] 除外 —— 它有意撑到屏幕右缘(命中区含边缘填充)。
    #[test]
    fn wide_char_columns_match_char_count() {
        let items = layout(40, &["a".into()], 0, false, &Config::default());
        for i in &items {
            let chars = i.text.chars().count();
            if i.action == Action::Hide {
                assert!(i.end - i.start >= chars, "右对齐项可含边缘填充");
                assert!(i.start + chars <= i.end);
            } else {
                assert_eq!(i.end - i.start, chars, "字符列宽必须等于字符数");
            }
        }
    }
}
