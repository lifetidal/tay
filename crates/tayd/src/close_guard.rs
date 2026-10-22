//! Close protection uses the shell's controlling terminal foreground process group.
//! Failure to inspect a live shell is treated conservatively as busy.
pub(crate) fn busy(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => busy_stat(&stat).unwrap_or(true),
        Err(e) => e.kind() != std::io::ErrorKind::NotFound,
    }
}
fn busy_stat(stat: &str) -> Option<bool> {
    let fields: Vec<_> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    // After comm: state, ppid, pgrp, session, tty_nr, tpgid.
    let pgrp = fields.get(2)?.parse::<i32>().ok()?;
    let tty = fields.get(4)?.parse::<i64>().ok()?;
    let foreground = fields.get(5)?.parse::<i32>().ok()?;
    Some(tty != 0 && foreground > 0 && foreground != pgrp)
}

fn dialog(cols: usize, rows: usize) -> (usize, usize, usize, usize) {
    let width = cols.saturating_sub(4).min(68).max(cols.min(4));
    let height = rows.min(5);
    ((cols - width) / 2, (rows - height) / 2, width, height)
}
pub(crate) fn hit(cols: usize, rows: usize, col: usize, row: usize) -> Option<bool> {
    let (x, y, w, h) = dialog(cols, rows);
    (h > 0 && row == y + h - 1 && col >= x && col < x + w).then_some(col >= x + w / 2)
}
pub(crate) fn paint(
    grid: &mut tay_render::Grid,
    config: &tay_config::Config,
    total: usize,
    busy: usize,
) {
    let (x, y, w, h) = dialog(grid.cols, grid.rows);
    let fg = config.colors.foreground;
    let bg = crate::settings::mix(config.tab_bar.background, fg, 0.06);
    let raised = crate::settings::mix(bg, fg, 0.10);
    for cell in &mut grid.cells {
        cell.bg = crate::settings::mix(cell.bg, bg, 0.7);
        cell.fg = crate::settings::mix(cell.fg, bg, 0.65);
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
        crate::settings::put(grid, row, x, x + w, "", fg, bg, false);
    }
    crate::settings::put(
        grid,
        y,
        x + 1,
        x + w.saturating_sub(1),
        &format!("关闭 {total} 个会话?"),
        fg,
        bg,
        true,
    );
    if h > 2 {
        crate::settings::put(
            grid,
            y + 2,
            x + 1,
            x + w.saturating_sub(1),
            &format!("将结束 {busy} 个正在运行的任务。"),
            fg,
            bg,
            false,
        );
    }
    if h > 0 && w > 1 {
        crate::settings::put(
            grid,
            y + h - 1,
            x,
            x + w / 2,
            " 取消 [Enter / Esc]",
            fg,
            raised,
            true,
        );
        crate::settings::put(
            grid,
            y + h - 1,
            x + w / 2,
            x + w,
            " 关闭 [Ctrl+Enter]",
            config.tab_bar.danger_foreground,
            raised,
            true,
        );
    }
    grid.opaque_background = true;
    grid.cursor_shape = tay_render::CursorShape::Hidden;
    grid.preedit = None;
    grid.selection = None;
    grid.overlays.clear();
    grid.images.clear();
    grid.window_icons.clear();
}

#[cfg(test)]
mod tests {
    #[test]
    fn dialog_hit_targets_match_visual_buttons() {
        for (cols, rows) in [(80, 24), (12, 3), (2, 1), (0, 0)] {
            let (x, y, w, h) = super::dialog(cols, rows);
            if w > 1 && h > 0 {
                assert_eq!(super::hit(cols, rows, x, y + h - 1), Some(false));
                assert_eq!(super::hit(cols, rows, x + w - 1, y + h - 1), Some(true));
                assert_eq!(super::hit(cols, rows, x + w, y + h - 1), None);
            }
        }
    }
    #[test]
    fn foreground_task_is_distinct_from_idle_shell_even_with_spaces_in_name() {
        assert_eq!(
            super::busy_stat("12 (fish (shell)) S 1 12 12 42 12 0"),
            Some(false)
        );
        assert_eq!(super::busy_stat("12 (fish) S 1 12 12 42 99 0"), Some(true));
        assert_eq!(super::busy_stat("12 (sh) S 1 12 12 0 -1 0"), Some(false));
        assert_eq!(super::busy_stat("invalid"), None);
    }
}
