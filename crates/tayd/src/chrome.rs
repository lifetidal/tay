//! 可见操作栏。布局与点击共享同一组范围，避免画面与命中不一致。
//!
//! 标签条支持 Lua 配色、单行布局与按显示列截断的 Unicode 标题。
use tay_config::{Config, Rgb};
use tay_render::{Cell, Grid};
use unicode_width::UnicodeWidthChar;

pub fn rows(config: &Config, settings: bool) -> usize {
    if !config.tab_bar.visible {
        // 极简模式:设置面板展开时临时恢复操作栏(关闭齿轮/时钟锚点
        // 仍在原位,面板钳在栏下);面板关闭即收回。
        if !settings {
            return 0;
        }
    }
    if config.tab_bar.compact { 2 } else { 3 }
}
fn truncate(text: &str, width: usize) -> String {
    let mut used = 0;
    text.chars()
        .filter(|c| !c.is_control())
        .take_while(|c| {
            used += c.width().unwrap_or(0);
            used <= width
        })
        .collect()
}
fn width(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Theme,
    Size,
    Opacity,
    Wallpaper,
    ImageOpacity,
    ImageBrightness,
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
    Blur,
    ResetAppearance,
    Diagnostics,
    SettingsPage(u8),
    Theme(u8),
    Level(Field, u8),
    PickWallpaper,
    ClearWallpaper,
    UndoAppearance,
    Smaller,
    Larger,
    Hide,
    Maximize,
    CloseWindow,
    Edit(Field),
    /// 时钟(右对齐,非交互;点击无操作)。
    Clock,
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

/// 标签栏占两行并垂直居中；展开设置后再显示两行控件。
/// 标签列宽按 Unicode 显示列计算，布局与点击共享范围。
pub fn layout(
    cols: usize,
    titles: &[String],
    active: usize,
    settings: bool,
    config: &Config,
    clock: &str,
) -> Vec<Item> {
    let mut items = Vec::new();
    let mut x = 0usize;

    // —— 行 0:标签页(分页;活动标签始终在本页)——
    let control_width = if cols >= 40 { 4 } else { 3 };
    let window_reserved = control_width * 3;
    let extra = if cols >= 18 { 6 } else { 0 };
    let clock_width = if !clock.is_empty() && cols >= window_reserved + extra + 14 {
        width(clock) + 2
    } else {
        0
    };
    let reserved = window_reserved + extra + clock_width;
    let usable = cols.saturating_sub(reserved);
    // 浏览器式收缩:标签多到一页放不下时,按总数均分宽度让
    // **全部标签始终可见**(标题截断);均分后仍不足 4 列才
    // 退回分页(活动标签所在页)。此前固定 tab_width 分页且
    // 无翻页提示 —— 用户看到"7 个到顶了",第 8 个像被顶掉。
    let count = titles.len().max(1);
    let mut tab_w = config.tab_bar.tab_width as usize;
    let page = (usable / tab_w).max(1);
    if count > page {
        // 每标签预算 =(可用宽 − 每标签 3 列关闭钮)/ 总数。
        let per = usable.saturating_sub(3 * count) / count;
        if per >= 4 {
            tab_w = per + 3;
        }
    }
    let page = (usable / tab_w).max(1);
    let first = (active / page) * page;
    for (i, title) in titles.iter().enumerate().skip(first).take(page) {
        let name = truncate(title, tab_w.saturating_sub(8));
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
        tab_text = truncate(&tab_text, available.min(tab_w - close_w));
        let tab_w = width(&tab_text);
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
        x += 1;
    }
    if extra > 0 {
        for (n, (text, action)) in [("   ", Action::New), ("   ", Action::Settings)]
            .into_iter()
            .enumerate()
        {
            let start = if n == 0 {
                x.min(cols - reserved)
            } else {
                cols - window_reserved - clock_width - 3
            };
            items.push(Item {
                row: 0,
                start,
                end: start + 3,
                text: text.into(),
                action,
                selected: settings && action == Action::Settings,
            });
        }
    }
    if cols >= window_reserved {
        if clock_width > 0 {
            let start = cols - window_reserved - clock_width;
            items.push(Item {
                row: 0,
                start,
                end: start + clock_width,
                text: format!(" {clock} "),
                action: Action::Clock,
                selected: false,
            });
        }
        for (index, action) in [Action::Hide, Action::Maximize, Action::CloseWindow]
            .into_iter()
            .enumerate()
        {
            let start = cols - window_reserved + index * control_width;
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

    items
}

/// 命中测试:`bar_rows` 内的行都映射到条目所在行(0) —— 标签块
/// 铺满整栏,栏内任意行点击都是同一组控件(设计 A:栏不再有消息行)。
pub fn hit(items: &[Item], bar_rows: usize, row: usize, col: usize) -> Option<Action> {
    let row = if row < bar_rows { 0 } else { row };
    items
        .iter()
        .find(|i| i.row == row && col >= i.start && col < i.end)
        .map(|i| i.action)
}

/// 按实际栏高平移终端内容、光标、选区、预编辑与图像。
pub fn decorate(
    grid: &mut Grid,
    items: &[Item],
    message: &str,
    config: &Config,
    hovered: Option<Action>,
    maximized: bool,
    settings: bool,
) {
    let rows = rows(config, settings);
    grid.tab_bar_rows = rows;
    let style = &config.tab_bar;
    let bg = style.background;
    let fg = style.active_foreground;
    let dim = style.inactive_foreground;
    let close = style.danger_foreground;
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
    let mut header = vec![base.clone(); rows * grid.cols];
    for item in items {
        if item.row >= rows {
            continue;
        }
        let is_tab = matches!(item.action, Action::Tab(_) | Action::Close(_));
        let mut style_cell = base.clone();
        style_cell.fg = if item.selected { fg } else { dim };
        style_cell.bg = if item.selected {
            style.active_background
        } else {
            if is_tab { style.hover_background } else { bg }
        };
        style_cell.bold = true;
        style_cell.underline = false;
        match item.action {
            Action::New | Action::Settings => style_cell.fg = style.icon_hover_foreground,
            Action::Close(_) | Action::Hide | Action::Maximize | Action::CloseWindow => {
                style_cell.fg = style.icon_foreground
            }
            _ => {}
        }
        if item.action == Action::Clock {
            style_cell.bg = style.clock_background;
            style_cell.fg = style.clock_foreground;
            style_cell.bold = true;
        }
        if hovered == Some(item.action) && item.action != Action::Clock {
            if matches!(item.action, Action::Close(_) | Action::CloseWindow) {
                style_cell.bg = blend(style_cell.bg, close, 0.22);
                style_cell.fg = close;
            } else if matches!(item.action, Action::Tab(_)) {
                style_cell.bg = if item.selected {
                    style.active_background
                } else {
                    style.hover_background
                };
                style_cell.fg = style.active_foreground;
            } else {
                style_cell.bg = style.hover_background;
                style_cell.fg = style.icon_hover_foreground;
            }
        } else if !is_tab && item.row > 0 {
            style_cell.fg = style.icon_foreground;
        }
        for col in item.start..item.end.min(grid.cols) {
            header[item.row * grid.cols + col] = style_cell.clone();
        }
        let mut col = item.start;
        for c in item.text.chars() {
            let w = c.width().unwrap_or(0);
            if w == 0 {
                continue;
            }
            if col + w > item.end.min(grid.cols) {
                break;
            }
            header[item.row * grid.cols + col].c = c;
            for offset in 1..w {
                header[item.row * grid.cols + col + offset].wide_spacer = true;
            }
            col += w;
        }
    }
    for item in items
        .iter()
        .filter(|i| i.row == 0 && matches!(i.action, Action::Tab(_) | Action::Clock))
    {
        // 时钟:独立高对比圆角色块(usability-review 验收设计),
        // 四角全圆、无活动标记、不受 hover 影响。
        if item.action == Action::Clock {
            grid.chrome_panels.push(tay_render::ChromePanel {
                row: 0,
                rows: 0,
                start: item.start,
                end: item.end,
                background: style.clock_background,
                corners: tay_render::PanelCorners::All,
                accent: None,
            });
            continue;
        }
        let end = if let Action::Tab(index) = item.action {
            items
                .iter()
                .find(|i| i.action == Action::Close(index))
                .map_or(item.end, |i| i.end)
        } else {
            item.end
        };
        grid.chrome_panels.push(tay_render::ChromePanel {
            row: 0,
            rows: 0,
            start: item.start,
            end,
            background: bg,
            corners: if matches!(item.action, Action::Tab(_)) {
                tay_render::PanelCorners::Opposite
            } else {
                tay_render::PanelCorners::All
            },
            accent: if item.selected {
                Some(style.accent)
            } else {
                None
            },
        });
    }
    // 帮助/编辑消息:整行弱化显示(占用栏的最后一行;栏最多 3 行,
    // 原 rows>=4 守卫永远不成立,消息从未画出)。
    // then(惰性):rows==0(tab_bar.visible=false)时不得计算 rows-1。
    let message_row = (!message.is_empty() && rows >= 2).then(|| rows - 1);
    // 有消息行时,行 0 的圆角面板只铺到消息行之上 —— 否则面板
    // (rows:0 语义 = 铺满 tab_bar_rows)盖进消息行,消息文字压在
    // 标签面板上(「全局快捷键…」行覆盖顶层标签的产品反馈)。
    grid.tab_panel_rows = rows.saturating_sub(usize::from(message_row.is_some()));
    if let Some(mrow) = message_row {
        for c in &mut header[mrow * grid.cols..(mrow + 1) * grid.cols] {
            *c = base.clone();
        }
        let mut col = 0;
        for c in message.chars() {
            let width = c.width().unwrap_or(1).max(1);
            if col + width > grid.cols {
                break;
            }
            header[mrow * grid.cols + col].c = c;
            header[mrow * grid.cols + col].fg = dim;
            for offset in 1..width {
                header[mrow * grid.cols + col + offset].wide_spacer = true;
            }
            col += width;
        }
    }
    if rows >= 2 {
        // 把首行样式复制到其余行(消息行除外,它已被整行弱化覆盖)。
        let copy_until = message_row.unwrap_or(rows);
        for col in 0..grid.cols {
            let mut cell = header[col].clone();
            cell.c = ' ';
            cell.zerowidth = None;
            cell.wide_spacer = false;
            for row in 1..copy_until {
                header[row * grid.cols + col] = cell.clone();
            }
        }
    }
    for item in items.iter().filter(|i| i.row == 0) {
        use tay_render::WindowIconKind;
        let kind = match item.action {
            Action::New => WindowIconKind::Add,
            Action::Settings => WindowIconKind::Settings,
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
    grid.rows += rows;
    grid.cursor.1 += rows;
    if let Some(s) = &mut grid.selection {
        s.start.1 += rows;
        s.end.1 += rows;
    }
    if let Some(p) = &mut grid.preedit {
        p.row += rows;
    }
    for b in &mut grid.badges {
        b.row += rows;
    }
    for o in &mut grid.overlays {
        o.row += rows;
    }
    for i in &mut grid.images {
        i.row += rows;
        if let Some((_, y, _, _)) = &mut i.clip {
            *y += rows;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clock_reserves_space_without_overlapping_tabs_or_settings() {
        let titles = vec!["中文 shell long".into(); 24];
        for cols in 2..200 {
            for active in [0, 5, 23] {
                let items = layout(cols, &titles, active, false, &Config::default(), "18:42");
                for (index, a) in items.iter().enumerate() {
                    assert!(a.end <= cols);
                    for b in items.iter().skip(index + 1) {
                        if a.row == b.row {
                            assert!(
                                a.end <= b.start || b.end <= a.start,
                                "cols={cols} {:?} {:?}",
                                a.action,
                                b.action
                            );
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn compact_header_uses_one_row_and_unicode_columns() {
        let config = Config::default();
        for cols in [2, 12, 18, 24, 40, 80, 160] {
            let items = layout(cols, &["中文 shell".into()], 0, false, &config, "");
            assert!(items.iter().all(|i| i.row == 0 && i.end <= cols));
            for item in &items {
                for col in item.start..item.end {
                    assert_eq!(hit(&items, 2, 0, col), Some(item.action));
                    assert_eq!(
                        hit(&items, 2, 1, col),
                        Some(item.action),
                        "加高区域下半部分可点击"
                    );
                }
            }
            if cols >= 24 {
                assert!(items.iter().any(|i| i.action == Action::New));
                assert!(items.iter().any(|i| i.action == Action::Settings));
            }
            for item in items {
                assert_eq!(width(&item.text), item.end - item.start);
            }
        }
        let items = layout(100, &["中文 shell".into()], 0, false, &config, "");
        assert!(items.iter().any(|i| i.text.contains("中文")));
        assert_eq!(rows(&config, false), 2);
        assert_eq!(rows(&config, true), 2);
    }

    #[test]
    fn minimal_mode_restores_the_bar_while_settings_are_open() {
        let mut config = Config::default();
        config.tab_bar.visible = false;
        // 极简模式平时无栏;设置面板展开时临时恢复(紧凑 2 行)。
        assert_eq!(rows(&config, false), 0);
        assert_eq!(rows(&config, true), 2);
        config.tab_bar.compact = false;
        assert_eq!(rows(&config, true), 3);
    }

    #[test]
    fn non_compact_bar_makes_all_three_rows_clickable() {
        let mut config = Config::default();
        config.tab_bar.compact = false;
        assert_eq!(rows(&config, false), 3);
        let items = layout(120, &["shell".into()], 0, false, &config, "18:42");
        let tab = items.iter().find(|i| i.action == Action::Tab(0)).unwrap();
        // 3 行都是同一组控件的点击延伸;栏外(第 3 行起)不得命中。
        for row in 0..3 {
            assert_eq!(hit(&items, 3, row, tab.start), Some(Action::Tab(0)));
        }
        assert_eq!(hit(&items, 3, 3, tab.start), None);
    }

    #[test]
    fn standard_window_controls_have_separate_hit_targets() {
        for cols in [24, 80, 200] {
            let items = layout(cols, &["shell".into()], 0, false, &Config::default(), "");
            let mut previous = 0;
            for action in [Action::Hide, Action::Maximize, Action::CloseWindow] {
                let item = items
                    .iter()
                    .find(|i| i.row == 0 && i.action == action)
                    .unwrap();
                assert!(item.start >= previous);
                assert_eq!(hit(&items, 2, 0, item.start), Some(action));
                assert_eq!(hit(&items, 2, 0, item.end - 1), Some(action));
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
                let items = layout(cols, &titles, active, true, &Config::default(), "");
                for action in [Action::Tab(active), Action::Close(active), Action::Settings] {
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
            let items = layout(80, &titles, active, false, &Config::default(), "");
            assert!(items.iter().any(|i| i.action == Action::Tab(active)));
        }
    }

    /// 浏览器式收缩:一页放不下时按总数均分,全部标签可见
    /// (用户实测"7 个到顶了"的修复 —— 此前固定 24 列分页,
    /// 200 列宽恰好一页 7 个,第 8 个起看不到)。
    #[test]
    fn all_tabs_shrink_to_fit_when_more_than_one_page() {
        // 200 列:固定 24 列只能放 7 个;收缩后 14 个全部可见。
        let titles: Vec<String> = (0..14).map(|i| format!("tab{i}")).collect();
        let items = layout(200, &titles, 13, false, &Config::default(), "");
        for i in 0..14 {
            assert!(
                items.iter().any(|x| x.action == Action::Tab(i)),
                "标签 {i} 必须可见(收缩而非分页)"
            );
        }
        assert!(items.iter().all(|i| i.end <= 200));
        // 窄到连 4 列都均分不出(80 列 × 24 标签)→ 退回分页,
        // 活动标签仍在本页(既有测试),且不越界。
        let many = vec!["s".to_string(); 24];
        let items = layout(80, &many, 23, false, &Config::default(), "");
        assert!(items.iter().any(|i| i.action == Action::Tab(23)));
        assert!(items.iter().all(|i| i.end <= 80));
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
                clip_right: None,
            }),
            badges: vec![],
            overlays: vec![],
            images: vec![],
            window_icons: vec![],
            chrome_panels: vec![],
            tab_bar_rows: 0,
            tab_panel_rows: 0,
            opaque_background: false,
            scrollbar: None,
        };
        decorate(
            &mut grid,
            &[],
            "help",
            &Config::default(),
            None,
            false,
            true,
        );
        assert_eq!(grid.rows, 3);
        assert_eq!(grid.cells.len(), 6);
        assert_eq!(grid.cells[4].c, 'z');
        assert_eq!(grid.cursor, (1, 2));
        assert_eq!(grid.preedit.unwrap().row, 2);
        assert_eq!(grid.selection.unwrap().start, (0, 2));
    }

    #[test]
    fn help_message_occupies_the_last_bar_row() {
        fn grid() -> Grid {
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
            Grid {
                cols: 40,
                rows: 1,
                cells: vec![cell; 40],
                cursor: (0, 0),
                cursor_shape: tay_render::CursorShape::Beam,
                selection: None,
                preedit: None,
                badges: vec![],
                overlays: vec![],
                images: vec![],
                window_icons: vec![],
                chrome_panels: vec![],
                tab_bar_rows: 0,
                tab_panel_rows: 0,
                opaque_background: false,
                scrollbar: None,
            }
        }
        let config = Config::default();
        let items = layout(40, &["shell".into()], 0, false, &config, "");
        let mut with_message = grid();
        decorate(
            &mut with_message,
            &items,
            "help",
            &config,
            None,
            false,
            false,
        );
        // compact 栏 2 行:消息占据最后一行,终端内容从其后开始。
        assert_eq!(with_message.rows, 3);
        assert_eq!(with_message.cells[40].c, 'h');
        assert_eq!(with_message.cells[41].c, 'e');
        assert_eq!(with_message.cells[42].c, 'l');
        assert_eq!(with_message.cells[43].c, 'p');
        assert_eq!(with_message.cells[80].c, 'z');
        // 有消息时行 0 面板只铺 1 行(2 − 1):圆角面板不得盖进消息行,
        // 否则消息文字压在标签面板上(「全局快捷键…覆盖顶层标签」回归)。
        assert_eq!(with_message.tab_panel_rows, 1);
        // 无消息时最后一行仍是首行样式的延伸(可点击高亮)。
        let mut plain = grid();
        decorate(&mut plain, &items, "", &config, None, false, false);
        assert_eq!(plain.cells[40].c, ' ');
        assert_eq!(plain.cells[80].c, 'z');
        assert_eq!(plain.tab_panel_rows, 2);
    }

    #[test]
    fn clock_uses_its_own_colors_and_rounded_panel() {
        let mut config = Config::default();
        config.tab_bar.clock_background = Rgb(10, 20, 30);
        config.tab_bar.clock_foreground = Rgb(200, 201, 202);
        let cols = 120;
        let items = layout(cols, &["shell".into()], 0, false, &config, "18:42");
        let clock = items
            .iter()
            .find(|i| i.action == Action::Clock)
            .expect("宽栏时钟可见");
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
            cols,
            rows: 1,
            cells: vec![cell; cols],
            cursor: (0, 0),
            cursor_shape: tay_render::CursorShape::Beam,
            selection: None,
            preedit: None,
            badges: vec![],
            overlays: vec![],
            images: vec![],
            window_icons: vec![],
            chrome_panels: vec![],
            tab_bar_rows: 0,
            tab_panel_rows: 0,
            opaque_background: false,
            scrollbar: None,
        };
        decorate(&mut grid, &items, "", &config, None, false, false);
        // 时钟格:独立高对比配色,而不是标签栏背景 + 图标前景。
        for col in clock.start..clock.end {
            assert_eq!(grid.cells[col].bg, config.tab_bar.clock_background);
            assert_eq!(grid.cells[col].fg, config.tab_bar.clock_foreground);
        }
        // 时钟圆角色块面板:clock_background、四角全圆、无活动标记。
        let panel = grid
            .chrome_panels
            .iter()
            .find(|p| p.start == clock.start && p.end == clock.end)
            .expect("时钟圆角面板");
        assert_eq!(panel.background, config.tab_bar.clock_background);
        assert_eq!(panel.corners, tay_render::PanelCorners::All);
        assert!(panel.accent.is_none());
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
                "",
            );
            for i in &items {
                assert!(i.end <= cols, "项不越界");
                for col in i.start..i.end {
                    assert_eq!(hit(&items, 2, i.row, col), Some(i.action));
                }
            }
        }
    }

    #[test]
    fn tab_close_is_separate_from_switch() {
        let items = layout(120, &["shell".into()], 0, false, &Config::default(), "");
        assert!(items.iter().any(|i| i.action == Action::Close(0)));
        assert!(items.iter().any(|i| i.action == Action::New));
    }

    /// 多字节字符(×)的列宽按字符数计:布局与渲染严格一致
    /// (旧实现按字节数计,× 会让后续列偏移 1 格)。右对齐的
    /// [Hide ×] 除外 —— 它有意撑到屏幕右缘(命中区含边缘填充)。
    #[test]
    fn wide_char_columns_match_char_count() {
        let items = layout(40, &["a".into()], 0, false, &Config::default(), "");
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
