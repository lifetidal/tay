//! Settings overlay: one layout drives painting, pointer hits and keyboard focus.
use crate::chrome::{Action, Field, Item};
use tay_config::{Config, Rgb};
use tay_render::{Cell, Grid};
use unicode_width::UnicodeWidthChar;
/// 窗口内快捷键清单(页 3 展示;binding_hint 取当前生效按键,
/// unbound 行跳过 —— 用户在 keys 里改键/禁用后实时反映)。
/// hide/maximize 不进清单:状态行已显示全局快捷键,窗口控制按钮
/// 就在栏上,重复展示徒增噪音。
const KEY_LABELS: &[(&str, &str)] = &[
    ("new-tab", "新标签"),
    ("close-tab", "关闭标签"),
    ("next-tab", "下一标签"),
    ("previous-tab", "上一标签"),
    ("copy", "复制"),
    ("paste", "粘贴"),
    ("settings", "设置"),
    ("split-right", "向右分屏"),
    ("split-down", "向下分屏"),
    ("next-pane", "下一窗格"),
    ("close-pane", "关闭窗格"),
    ("zoom-pane", "放大窗格"),
];
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
/// 面板矩形:顶部**不得进入操作栏**(bar_rows)—— 矮窗口时若按
/// 居中公式,面板会盖住栏(齿轮/标签被遮挡,且指针命中与绘制
/// 分离);可用高度 = 总行数 − 栏高,上限 16。
pub(crate) fn rect(cols: usize, rows: usize, bar_rows: usize) -> (usize, usize, usize, usize) {
    let w = cols.saturating_sub(4).min(88).max(cols.min(4));
    // 可用高度 = 总行数 − 栏高(最小网格 3 行,栏 2 行 → h ≥ 1);
    // 退化网格(行数 ≤ 栏高)时钳回网格内,保证 item 永不越界。
    let h = rows.saturating_sub(bar_rows).min(16);
    let y = ((rows - h) / 2).max(bar_rows).min(rows.saturating_sub(1));
    ((cols - w) / 2, y, w, h)
}
pub(crate) fn layout(cols: usize, rows: usize, page: u8, c: &Config) -> Vec<Item> {
    // 面板永远从栏之下开始(栏高与调用方一致:bar_rows = tab_bar 行数)。
    let bar_rows = crate::chrome::rows(c, true);
    let (x, y, w, h) = rect(cols, rows, bar_rows);
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
            add(
                y + base + 4 * step,
                left,
                right,
                format!(" 栏装饰: {} ⟳", c.tab_bar.pattern.label()),
                Action::Pattern,
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
            // 状态行(y+3)与窗口内快捷键清单(y+5 起)由 paint 直接绘制
            // (展示性内容不进布局,键盘焦点不会停在不可交互的行上)。
            add(
                y + 4,
                left,
                mid,
                " 配置桌面快捷键 > ".into(),
                Action::Hotkeys,
                false,
            );
            add(
                y + 4,
                mid,
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
    reveal: f32,
    info: &OverlayInfo<'_>,
) {
    let (x, y, w, h) = rect(grid.cols, grid.rows, crate::chrome::rows(c, true));
    // 开合动画:面板从栏向下展开 / 向上收起(整行粒度)。reveal==0
    // 时不推面板不画任何行 —— ChromePanel 的 rows:0 是「铺满整栏」
    // 语义,绝不能把空面板当标签面板画。
    let visible_h = ((h as f32 * reveal).ceil() as usize).min(h);
    let fg = c.colors.foreground;
    // 面板底色取终端配色而非 tab_bar:栏可能被手写定制成与主题无关
    // 的颜色(亮色终端配深色栏时面板深底深字,产品反馈 2026-11-10)。
    let backdrop = c.colors.background;
    let bg = mix(backdrop, fg, 0.06);
    let muted = mix(fg, bg, 0.35);
    let raised = mix(bg, fg, 0.10);
    let accent = c.tab_bar.accent;
    for cell in &mut grid.cells {
        cell.bg = mix(cell.bg, backdrop, 0.65 * reveal);
        cell.fg = mix(cell.fg, bg, 0.65 * reveal);
    }
    let surround = grid
        .cells
        .get(y * grid.cols + x)
        .map(|cell| cell.bg)
        .unwrap_or(bg);
    if visible_h > 0 {
        grid.chrome_panels.push(tay_render::ChromePanel {
            row: y,
            rows: visible_h,
            start: x,
            end: x + w,
            background: surround,
            accent: None,
            corners: tay_render::PanelCorners::All,
        });
    }
    for row in y..y + visible_h {
        put(grid, row, x, x + w, "", fg, bg, false);
    }
    if visible_h >= 1 {
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
        // 标题与「关闭」钮之间补一条弱化分隔线(与栏内装饰线同一语言;
        // 关闭钮稍后在 item 循环里绘制,会覆盖线尾,无需预留边界)。
        let rule_start = x + 6;
        let rule_end = (x + w).saturating_sub(8);
        if rule_start + 2 < rule_end {
            let rule_fg = mix(muted, bg, 0.5);
            for col in rule_start..rule_end {
                let cell = &mut grid.cells[y * grid.cols + col];
                cell.c = '─';
                cell.fg = rule_fg;
                cell.bg = bg;
                cell.bold = false;
                cell.wide_spacer = false;
                cell.zerowidth = None;
            }
        }
    }
    // 设计 A:设置提示从栏消息行移进面板内(标题与页签之间的一行):
    // 常驻提示跟随设置上下文,编辑字段时同位置切换为编辑帮助。
    // 面板太矮时不画(h < 5:提示行会挤掉全部内容行,退化布局里
    // 宁可没有提示也要保住按钮)。
    if h >= 5 && y + 1 < y + visible_h {
        let hint = if let Some((field, value)) = editing {
            format!(
                "编辑 {}: {value}_  Enter=保存  Esc=取消  Ctrl+A=替换",
                field_label(field)
            )
        } else {
            // 按页提示:只讲本页语义,不把别的页的信息掺进来;
            // 尾部固定面板内键盘路径(Tab 循环焦点 / Esc 关闭)。
            match page {
                0 => "外观立即生效并保存；Ctrl+Z 撤销上次修改 ·Tab 切换·Esc 关闭",
                1 => "背景图留空=禁用；滑条拖动即时生效 ·Tab 切换·Esc 关闭",
                _ => "全局快捷键修改需重启授权；窗口内快捷键即时生效 ·Tab 切换·Esc 关闭",
            }
            .into()
        };
        put(grid, y + 1, x + 2, x + w - 2, &hint, muted, bg, false);
    }
    let items = layout(grid.cols, grid.rows, page, c);
    for item in &items {
        if item.row >= y + visible_h {
            continue;
        }
        let unavailable = matches!(item.action, Action::UndoAppearance) && !info.undo_available;
        let active = item.selected || (focus == Some(item.action) && !unavailable);
        let mut color = if unavailable {
            mix(raised, bg, 0.5)
        } else if active {
            mix(bg, accent, 0.35)
        } else {
            raised
        };
        // 「关闭」钮:悬停/聚焦转危险色(与栏内 × 一致),不涂强调色。
        if item.action == Action::Settings && active {
            color = mix(bg, c.tab_bar.danger_foreground, 0.30);
        }
        let (button_fg, button_bg, text_start) = if let Action::Theme(index) = item.action {
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
                item.start + 2,
            )
        } else if unavailable {
            (muted, color, item.start)
        } else {
            (fg, color, item.start)
        };
        put(
            grid, item.row, text_start, item.end, &item.text, button_fg, button_bg, active,
        );
        if let Action::Theme(index) = item.action {
            // 主题瓷砖:名称前加光标色圆点 —— 一眼看出主题的强调色。
            let palette = tay_config::Colors::preset(
                ["default", "midnight", "light", "wezterm"][index as usize],
            )
            .unwrap_or_else(|| c.colors.clone());
            let dot = item.start + 1;
            if dot < item.end.min(grid.cols) {
                let cell = &mut grid.cells[item.row * grid.cols + dot];
                cell.c = '●';
                cell.fg = palette.cursor;
                cell.bg = button_bg;
                cell.bold = false;
                cell.wide_spacer = false;
                cell.zerowidth = None;
            }
        }
        if let Action::SettingsPage(_) = item.action
            && item.selected
        {
            // 活动页签:与栏内活动标签同语言的圆角芯片 + 强调边。
            grid.chrome_panels.push(tay_render::ChromePanel {
                row: item.row,
                rows: 1,
                start: item.start,
                end: item.end,
                background: mix(bg, accent, 0.35),
                accent: Some(accent),
                corners: tay_render::PanelCorners::All,
            });
        }
    }
    if page == 2 && h >= 5 {
        // 全局快捷键授权状态固定 y+3;窗口内快捷键清单从 y+5 起,
        // 每行两个 —— 展示性内容不进布局,键盘焦点不停在不可
        // 交互的行上。行超面板即停(整行粒度,不截半行)。
        let shortcut = info
            .status
            .split('\t')
            .find_map(|s| s.strip_prefix("shortcut="))
            .unwrap_or("快捷键绑定待确认");
        if y + 3 < y + visible_h {
            put(grid, y + 3, x + 2, x + w - 2, shortcut, muted, bg, false);
        }
        let cell = (x + w - 2).saturating_sub(x + 2) / 2;
        let mut slot = 0;
        for (action, label) in KEY_LABELS {
            let key = c.binding_hint(action);
            if key == "unbound" {
                continue;
            }
            let row = y + 5 + slot / 2;
            if row >= y + h - 1 || row >= y + visible_h {
                break;
            }
            let col = x + 2 + (slot % 2) * cell;
            put(
                grid,
                row,
                col,
                col + cell.saturating_sub(1),
                &format!(" {label}  {key} "),
                fg,
                bg,
                false,
            );
            slot += 1;
        }
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
        // 编辑态底栏只回显正在编辑的值;Enter/Esc/Ctrl+A 键位提示
        // 归面板提示行(设计审查:去掉与提示行的重复,底栏=状态)。
        format!("{}: {value}_", field_label(field))
    } else if page == 1
        && !c.background_image.is_empty()
        && let Some(error) = info.background_error
    {
        format!("图片加载失败:{error}")
    } else if page == 1 && !c.background_image.is_empty() && c.background_image_opacity >= 1.0 {
        // 图片强度 100% 时壁纸完全不透明:窗口不透明度只作用于底色,
        // 调它压不暗壁纸(终端文字绘制在壁纸之上,不会被遮住)。
        "图片强度 100%:壁纸不透明,窗口不透明度不再影响它,请调低「图片强度」".into()
    } else if !info.feedback.is_empty() {
        info.feedback.into()
    } else {
        // 导航键位提示归面板提示行(y+1),底栏只留状态(设计审查去重)。
        "已自动保存到 Lua".into()
    };
    if h > 1 {
        put(grid, y + h - 1, x + 1, x + w - 1, &footer, muted, bg, false);
    }
    grid.opaque_background = true;
    grid.cursor_shape = tay_render::CursorShape::Hidden;
    grid.preedit = None;
    grid.selection = None;
    grid.search_hit = None;
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
    #[test]
    fn hint_row_shows_settings_hint_and_switches_to_editing_help() {
        let c = Config::default();
        let (cols, rows) = (80, 20);
        let cell = tay_render::Cell {
            c: ' ',
            fg: tay_config::Rgb(255, 255, 255),
            bg: tay_config::Rgb(0, 0, 0),
            bold: false,
            italic: false,
            underline: false,
            wide_spacer: false,
            zerowidth: None,
        };
        let make_grid = || tay_render::Grid {
            cols,
            rows,
            cells: vec![cell.clone(); cols * rows],
            cursor: (0, 0),
            cursor_shape: tay_render::CursorShape::Beam,
            selection: None,
            search_hit: None,
            preedit: None,
            badges: vec![],
            overlays: vec![],
            images: vec![],
            window_icons: vec![],
                wave: vec![],
            chrome_panels: vec![],
            tab_bar_rows: 0,
            tab_panel_rows: 0,
            opaque_background: false,
            scrollbar: None,
        };
        let info = OverlayInfo {
            status: "",
            feedback: "",
            background_error: None,
            undo_available: false,
        };
        // 常规态:标题下一行(栏下 y+1)是设置提示。
        let mut grid = make_grid();
        paint(&mut grid, &c, 0, None, None, 1.0, &info);
        let (_, y, _, _) = rect(cols, rows, crate::chrome::rows(&c, true));
        let row_text: String = grid.cells[(y + 1) * cols..(y + 2) * cols]
            .iter()
            .filter(|c| !c.wide_spacer)
            .map(|c| c.c)
            .collect();
        assert!(row_text.contains("外观立即生效"));
        // 编辑态:同一行切换为编辑帮助,含字段名与保存/取消键。
        let mut grid = make_grid();
        paint(
            &mut grid,
            &c,
            0,
            None,
            Some(&(Field::Font, "JetBrainsMono".into())),
            1.0,
            &info,
        );
        let row_text: String = grid.cells[(y + 1) * cols..(y + 2) * cols]
            .iter()
            .filter(|c| !c.wide_spacer)
            .map(|c| c.c)
            .collect();
        assert!(row_text.contains("编辑 字体"));
        assert!(row_text.contains("Enter=保存"));
        // 矮面板(h < 5)不画提示行,内容行保留。
        let mut grid = make_grid();
        grid.rows = crate::chrome::rows(&c, true) + 4;
        paint(&mut grid, &c, 0, None, None, 1.0, &info);
        let (_, y, _, _) = rect(cols, grid.rows, crate::chrome::rows(&c, true));
        let row_text: String = grid.cells[(y + 1) * cols..(y + 2) * cols]
            .iter()
            .filter(|c| !c.wide_spacer)
            .map(|c| c.c)
            .collect();
        assert!(!row_text.contains("外观立即生效"));
    }
    #[test]
    fn shortcuts_page_lists_effective_bindings_from_config() {
        let c = Config::default();
        let (cols, rows) = (80, 20);
        let cell = tay_render::Cell {
            c: ' ',
            fg: tay_config::Rgb(255, 255, 255),
            bg: tay_config::Rgb(0, 0, 0),
            bold: false,
            italic: false,
            underline: false,
            wide_spacer: false,
            zerowidth: None,
        };
        let mut grid = tay_render::Grid {
            cols,
            rows,
            cells: vec![cell; cols * rows],
            cursor: (0, 0),
            cursor_shape: tay_render::CursorShape::Beam,
            selection: None,
            search_hit: None,
            preedit: None,
            badges: vec![],
            overlays: vec![],
            images: vec![],
            window_icons: vec![],
                wave: vec![],
            chrome_panels: vec![],
            tab_bar_rows: 0,
            tab_panel_rows: 0,
            opaque_background: false,
            scrollbar: None,
        };
        let info = OverlayInfo {
            status: "shortcut=CTRL+grave",
            feedback: "",
            background_error: None,
            undo_available: false,
        };
        paint(&mut grid, &c, 2, None, None, 1.0, &info);
        let (x, y, w, h) = rect(cols, rows, crate::chrome::rows(&c, true));
        // 状态行在 y+3,显示 health 快照里的快捷键授权状态。
        let status: String = grid.cells[(y + 3) * cols..(y + 4) * cols]
            .iter()
            .filter(|c| !c.wide_spacer)
            .map(|c| c.c)
            .collect();
        assert!(status.contains("CTRL+grave"));
        // 清单从 y+5 起,默认配置的 new-tab 绑定是 Ctrl+t。
        let first: String = grid.cells[(y + 5) * cols..(y + 6) * cols]
            .iter()
            .filter(|c| !c.wide_spacer)
            .map(|c| c.c)
            .collect();
        assert!(
            first.contains("新标签") && first.contains("Ctrl+t"),
            "{first}"
        );
        assert_eq!(h, 16.min(rows - crate::chrome::rows(&c, true)));
        let _ = (x, w);
        // 清单行不越出面板(整行粒度)。
        let last_list_row = y + 5 + KEY_LABELS.len() / 2;
        assert!(last_list_row < y + h);
    }
    #[test]
    fn reveal_clips_paint_by_rows_and_zero_reveal_pushes_no_panel() {
        let c = Config::default();
        let (cols, rows) = (80, 20);
        let cell = tay_render::Cell {
            c: 'z',
            fg: tay_config::Rgb(255, 255, 255),
            bg: tay_config::Rgb(0, 0, 0),
            bold: false,
            italic: false,
            underline: false,
            wide_spacer: false,
            zerowidth: None,
        };
        let make_grid = || tay_render::Grid {
            cols,
            rows,
            cells: vec![cell.clone(); cols * rows],
            cursor: (0, 0),
            cursor_shape: tay_render::CursorShape::Beam,
            selection: None,
            search_hit: None,
            preedit: None,
            badges: vec![],
            overlays: vec![],
            images: vec![],
            window_icons: vec![],
                wave: vec![],
            chrome_panels: vec![],
            tab_bar_rows: 0,
            tab_panel_rows: 0,
            opaque_background: false,
            scrollbar: None,
        };
        let info = OverlayInfo {
            status: "shortcut=x",
            feedback: "",
            background_error: None,
            undo_available: false,
        };
        let (_, y, _, h) = rect(cols, rows, crate::chrome::rows(&c, true));
        // 半开:仅 y..y+ceil(h/2) 被画(该范围外保留 'z' 原格)。
        let mut grid = make_grid();
        paint(&mut grid, &c, 0, None, None, 0.5, &info);
        let visible_h = ((h as f32 * 0.5).ceil() as usize).min(h);
        // 活动页签的芯片面板可能随半开面板出现;首个面板必须是包围面板。
        assert!(!grid.chrome_panels.is_empty());
        assert_eq!(grid.chrome_panels[0].rows, visible_h);
        assert_eq!(grid.cells[(y + visible_h) * cols].c, 'z');
        // reveal=0:不推面板(rows:0 是「铺满整栏」语义,不能误用)。
        let mut grid = make_grid();
        paint(&mut grid, &c, 0, None, None, 0.0, &info);
        assert!(grid.chrome_panels.is_empty());
    }
}
