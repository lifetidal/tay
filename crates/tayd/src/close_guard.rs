//! Close protection uses the shell's controlling terminal foreground process group.
//! Failure to inspect a live shell is treated conservatively as busy.
pub(crate) fn busy(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => {
            let Some((tty, foreground, pgrp)) = busy_fields(&stat) else {
                return true;
            };
            // 先短路再走子进程扫描:前台任务组占用 tty / 无 tty 时
            // 根本不用读 /proc 树(扫描只在 shell 持有 tty 时有意义)。
            if tty == 0 || foreground <= 0 {
                return false;
            }
            if foreground != pgrp {
                return true;
            }
            live_children(pid)
        }
        Err(e) => e.kind() != std::io::ErrorKind::NotFound,
    }
}

/// shell 是否有会随终端一起死掉的直接或间接子进程。
/// Ctrl+Z 挂起的任务把终端交还 shell(tpgid 回到 shell),后台任务
/// 也从不上前台 —— 仅看前台进程组会把它们漏掉,关闭时静默杀死。
/// 忽略 SIGHUP 的进程(nohup 等)终端关闭也杀不死,不算忙碌:
/// 把常驻守护进程算进去会让确认框频繁误报,用户养成盲目
/// Ctrl+Enter 的习惯,真正需要确认的任务反而失去保护。
fn live_children(root: u32) -> bool {
    fn children(pid: u32) -> Vec<u32> {
        std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
            .ok()
            .map(|text| {
                text.split_whitespace()
                    .filter_map(|t| t.parse::<u32>().ok())
                    .collect()
            })
            .unwrap_or_default()
    }
    fn state(pid: u32) -> Option<char> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat.rsplit_once(')')?
            .1
            .split_whitespace()
            .next()?
            .chars()
            .next()
    }
    /// SigIgn 掩码 bit N = 信号 N+1:bit 0 = SIGHUP(nohup 等)。
    fn hup_ignored(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()
            .and_then(|status| {
                status.lines().find_map(|line| {
                    line.strip_prefix("SigIgn:")
                        .and_then(|rest| rest.split_whitespace().next())
                        .and_then(|hex| u64::from_str_radix(hex, 16).ok())
                })
            })
            .is_some_and(|mask| mask & 1 != 0)
    }
    let mut stack = children(root);
    let mut visited = std::collections::HashSet::new();
    visited.insert(root);
    while let Some(pid) = stack.pop() {
        if !visited.insert(pid) {
            continue;
        }
        match state(pid) {
            Some('Z') | Some('X') | None => {}
            // 挂起任务:无论如何都算(关闭会杀死它们)。
            Some('T') => return true,
            // 运行/睡眠:忽略 HUP 的进程(nohup 守护)不算。
            Some(_) if !hup_ignored(pid) => return true,
            Some(_) => {}
        }
        stack.extend(children(pid));
    }
    false
}

/// 解析 /proc/pid/stat 的 (tty_nr, tpgid, pgrp);解析失败 → None(保守 busy)。
fn busy_fields(stat: &str) -> Option<(i64, i32, i32)> {
    let fields: Vec<_> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    // After comm: state, ppid, pgrp, session, tty_nr, tpgid.
    let pgrp = fields.get(2)?.parse::<i32>().ok()?;
    let tty = fields.get(4)?.parse::<i64>().ok()?;
    let foreground = fields.get(5)?.parse::<i32>().ok()?;
    Some((tty, foreground, pgrp))
}

/// 生产路径用 busy_fields 短路(见 busy);此函数仅供测试覆盖
/// 字段解析与决策组合。
#[cfg(test)]
fn busy_stat(stat: &str, live_children: bool) -> Option<bool> {
    let (tty, foreground, pgrp) = busy_fields(stat)?;
    Some(tty != 0 && foreground > 0 && (foreground != pgrp || live_children))
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
            super::busy_stat("12 (fish (shell)) S 1 12 12 42 12 0", false),
            Some(false)
        );
        assert_eq!(
            super::busy_stat("12 (fish) S 1 12 12 42 99 0", false),
            Some(true)
        );
        assert_eq!(
            super::busy_stat("12 (sh) S 1 12 12 0 -1 0", true),
            Some(false)
        );
        assert_eq!(super::busy_stat("invalid", false), None);
    }
    #[test]
    fn stopped_or_background_jobs_count_as_busy_even_when_shell_owns_the_terminal() {
        // tpgid 回到 shell(Ctrl+Z / 后台任务):无子进程 = 空闲。
        assert_eq!(
            super::busy_stat("12 (fish) S 1 12 12 42 12 0", false),
            Some(false)
        );
        // 有存活/挂起子进程 = 忙碌(关闭确认不得漏掉)。
        assert_eq!(
            super::busy_stat("12 (fish) S 1 12 12 42 12 0", true),
            Some(true)
        );
    }
    #[test]
    fn live_children_finds_shell_jobs_and_leaf_processes_have_none() {
        let mut job = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        assert!(!super::live_children(job.id()), "无子进程的任务不算忙碌");
        let _ = job.kill();
        let _ = job.wait();
        let mut shell = std::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 2 & sleep 5")
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !super::live_children(shell.id()) {
            assert!(
                std::time::Instant::now() < deadline,
                "shell 的后台任务未被发现"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = shell.kill();
        let _ = shell.wait();
        // nohup 子进程忽略 SIGHUP,终端关闭杀不死它:不算忙碌,
        // 否则确认框会把常驻守护进程误报成「正在运行的任务」。
        // shell 用 `wait` 保持存活,子进程不被 reparent —— 否则
        // 非交互 shell 退出后子树消失,断言失去判别力(四轮 review
        // 实测:旧测试因此放过了掩码位错)。
        let mut detached = std::process::Command::new("sh")
            .arg("-c")
            .arg("nohup sleep 5 & wait")
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            !super::live_children(detached.id()),
            "nohup 子进程忽略 SIGHUP,不应计为忙碌"
        );
        let _ = detached.kill();
        let _ = detached.wait();
        // 对照:普通后台子进程(不忽略 HUP)应计为忙碌。
        let mut plain = std::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 5 & wait")
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !super::live_children(plain.id()) {
            assert!(
                std::time::Instant::now() < deadline,
                "普通后台任务应计为忙碌"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = plain.kill();
        let _ = plain.wait();
    }
}
