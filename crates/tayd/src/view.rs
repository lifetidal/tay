//! TerminalView(架构 §2.3):可视区域与折叠投影。
//!
//! 网格行 ⇄ 显示行的**正向/反向映射唯一实现** —— 绘制、鼠标命中、
//! 选区、QuickSelect、光标与 IME 共用。折叠只影响显示,不修改
//! 终端底层内容;折叠占位行代表整个折叠区间(含端点)。
//!
//! 下方四个纯函数族(折叠区间/选区/徽章/图像投影)原本内联在
//! app.build_grid(架构 §6 阶段 3 的未完成项);现收拢于此:
//! 全部坐标转换唯一实现点 = session.rs / view.rs。

use std::collections::BTreeSet;

use crate::blocks::{BlockId, CommandBlockStore};
use crate::session::{DisplayPoint, GridPoint, HistoryAnchor, KittyImageEntry, SessionMeta};

/// 一个显示行的条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewEntry {
    /// 网格首行(占位行 = 折叠区间首行)。
    pub start: i32,
    /// 网格末行(含端点;普通行 == start;占位行 = 折叠区间末行)。
    pub end: i32,
    /// 是否折叠占位行。
    pub marker: bool,
    /// 占位文本(如「⋯ 3 行已折叠」)。
    pub text: String,
}

/// 可见网格范围 → 显示行的映射。
#[derive(Debug, Clone, Default)]
pub struct ViewMap {
    pub entries: Vec<ViewEntry>,
    /// 构建本映射所用快照的 SessionMeta(网格行 → 绝对锚点转换
    /// 与映射同源;显示坐标在**映射所在版本**上定格)。
    pub meta: crate::session::SessionMeta,
}

impl ViewMap {
    /// 携带构建版本(SessionMeta);锚点转换必须与映射同源
    /// (P1:异步复制不得混用新旧版本)。
    pub fn with_meta(mut self, meta: crate::session::SessionMeta) -> Self {
        self.meta = meta;
        self
    }
}

impl ViewMap {
    /// `first..=last` 为可见网格行;`ranges` 为已解析到 grid 空间的
    /// 折叠区间(相邻提示符之间、被折叠的输出行,含端点,有序)。
    pub fn new(first: i32, last: i32, ranges: &[(i32, i32)]) -> Self {
        let mut entries = Vec::new();
        let mut line = first;
        while line <= last {
            if let Some((a, b)) = ranges.iter().find(|(a, b)| line >= *a && line <= *b) {
                let n = b - a + 1;
                entries.push(ViewEntry {
                    start: *a,
                    end: *b,
                    marker: true,
                    text: format!("⋯ {n} 行已折叠"),
                });
                line = b + 1;
            } else {
                entries.push(ViewEntry {
                    start: line,
                    end: line,
                    marker: false,
                    text: String::new(),
                });
                line += 1;
            }
        }
        Self {
            entries,
            meta: crate::session::SessionMeta::default(),
        }
    }

    /// 显示行 → 网格行(占位行返回折叠区间首行 —— 命中测试沿用)。
    pub fn row_to_grid(&self, row: usize) -> Option<i32> {
        self.entries.get(row).map(|e| e.start)
    }

    /// 网格行 → 显示行。折叠区间内任意行落到占位行;
    /// 可见范围之外 → None。
    pub fn grid_to_row(&self, line: i32) -> Option<usize> {
        self.entries
            .iter()
            .position(|e| line >= e.start && line <= e.end)
    }

    /// Cursor must disappear outside the viewport or inside a folded marker.
    pub fn cursor_position(&self, p: GridPoint, cols: usize) -> Option<DisplayPoint> {
        let row = self.grid_to_row(p.line)?;
        if self.entries[row].marker || p.col as usize >= cols {
            return None;
        }
        Some(DisplayPoint {
            row,
            col: p.col as usize,
        })
    }

    /// 网格点 → 显示点(折叠感知;可见范围之外钳到最后一行,
    /// 与原语义一致 —— 光标滚出视口时仍给出可渲染位置)。
    pub fn project(&self, p: GridPoint, cols: usize) -> DisplayPoint {
        let row = self
            .grid_to_row(p.line)
            .unwrap_or(self.entries.len().saturating_sub(1));
        DisplayPoint {
            row,
            col: (p.col as usize).min(cols.saturating_sub(1)),
        }
    }
}

/// 折叠区间(绝对锚点)→ 网格行区间。早于历史淘汰的区间直接
/// 丢弃(显示层不渲染失效锚点);解析走 SessionMeta::resolve
/// 唯一转换点(架构 §4.2)。
pub fn fold_ranges_to_grid(
    blocks: &CommandBlockStore,
    folded: &BTreeSet<BlockId>,
    meta: SessionMeta,
    total_lines: usize,
    rows: usize,
) -> Vec<(i32, i32)> {
    blocks
        .fold_ranges(folded)
        .into_iter()
        .filter_map(|(a, b)| {
            let ga = meta.resolve(a, total_lines, rows)?;
            let gb = meta.resolve(b, total_lines, rows)?;
            Some((ga, gb))
        })
        .collect()
}

/// 网格点 → 显示坐标(vi 选区投影;折叠区间内落到占位行)。
fn project_point(
    p: alacritty_terminal::index::Point,
    view: &ViewMap,
    cols: usize,
) -> (usize, usize) {
    let d = view.project(
        GridPoint {
            line: p.line.0,
            col: p.column.0 as u32,
        },
        cols,
    );
    (d.col, d.row)
}

/// 选区 → 显示空间(折叠感知)。鼠标拖选(UI 视图状态,选择时刻
/// 定格锚点)优先;无鼠标选区时用快照的 vi 选区(会话侧)。
/// 同一行的列序归一(反向拖选)。
pub fn selection_to_display(
    mouse: Option<((HistoryAnchor, u32), (HistoryAnchor, u32))>,
    snap_selection: Option<alacritty_terminal::selection::SelectionRange>,
    meta: SessionMeta,
    view: &ViewMap,
    cols: usize,
) -> Option<tay_render::SelectionRange> {
    mouse
        .and_then(|((sl, sc), (el, ec))| {
            let (a0, a1) = if sl <= el {
                ((sl, sc), (el, ec))
            } else {
                ((el, ec), (sl, sc))
            };
            let row0 = view.grid_to_row(a0.0.0 - meta.cum_scroll)?;
            let row1 = view.grid_to_row(a1.0.0 - meta.cum_scroll)?;
            let (r0, r1) = if row0 <= row1 {
                (row0, row1)
            } else {
                (row1, row0)
            };
            // 同一行的列序归一(反向拖选)。
            let (c0, c1) = if r0 == r1 && a0.1 > a1.1 {
                (a1.1, a0.1)
            } else {
                (a0.1, a1.1)
            };
            Some(tay_render::SelectionRange {
                start: ((c0 as usize).min(cols - 1), r0),
                end: ((c1 as usize).min(cols - 1), r1),
            })
        })
        .or_else(|| {
            snap_selection.map(|s| tay_render::SelectionRange {
                start: project_point(s.start, view, cols),
                end: project_point(s.end, view, cols),
            })
        })
}

/// OSC 133 徽章(绝对行)→ 显示行 + 文本(非零退出码显示在对应
/// 提示符行尾;块状态来自快照的 COW 共享引用,与网格同版本)。
pub fn badges_to_display(
    blocks: &CommandBlockStore,
    meta: SessionMeta,
    total_lines: usize,
    rows: usize,
    view: &ViewMap,
) -> Vec<tay_render::Badge> {
    blocks
        .badges()
        .filter_map(|(line, code)| {
            let gl = meta.resolve(*line, total_lines, rows)?;
            let row = view.grid_to_row(gl)?;
            Some(tay_render::Badge {
                row,
                text: format!(" ✗ {code} "),
                color: tay_config::Rgb::new(0xff, 0x5f, 0x5f),
            })
        })
        .collect()
}

/// kitty 图像放置(网格行锚点)→ 显示行;跨折叠区间 → 隐藏
/// (修复:此前误把绝对行与 grid 相对区间比较,滚动后折叠隐藏
/// 失效 —— 这里保持「绝对行 + 折叠区间」判定)。
pub fn images_to_display(
    images: &[KittyImageEntry],
    meta: SessionMeta,
    total_lines: usize,
    rows: usize,
    ranges: &[(i32, i32)],
    view: &ViewMap,
    cell_size: (f32, f32),
) -> Vec<tay_render::RenderImage> {
    let (cw, ch) = cell_size;
    images
        .iter()
        .filter_map(|e| {
            // 图像锚点同走唯一转换点;已被历史淘汰 → 不渲染。
            let gl = meta.resolve(HistoryAnchor(e.grid_line), total_lines, rows)?;
            let row = view.grid_to_row(gl)?;
            let img_cols = e
                .img
                .cols
                .unwrap_or((e.img.w as f32 / cw).ceil() as u32)
                .max(1);
            let img_rows = e
                .img
                .rows
                .unwrap_or((e.img.h as f32 / ch).ceil() as u32)
                .max(1);
            if ranges
                .iter()
                .any(|(a, b)| gl + img_rows as i32 > *a && gl <= *b)
            {
                return None;
            }
            Some(tay_render::RenderImage {
                id: e.id,
                row,
                col: e.col as usize,
                cols: img_cols,
                rows: img_rows,
                w: e.img.w,
                h: e.img.h,
                z: e.img.z,
                rgba: e.img.rgba.clone(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(p: (i32, u32)) -> GridPoint {
        GridPoint {
            line: p.0,
            col: p.1,
        }
    }

    /// 折叠映射:区间内 → 占位行;区间后 → 各自行(边界修复:
    /// 折叠区间之后的第一个提示符行必须映射到自己的显示行,
    /// 不能落进占位行)。
    #[test]
    fn cursor_is_not_clamped_into_history_or_fold_markers() {
        let m = ViewMap::new(-10, -1, &[]);
        assert_eq!(m.cursor_position(grid((0, 2)), 80), None);
        assert_eq!(
            m.cursor_position(grid((-2, 2)), 80),
            Some(DisplayPoint { row: 8, col: 2 })
        );
        let m = ViewMap::new(0, 8, &[(2, 4)]);
        assert_eq!(m.cursor_position(grid((3, 2)), 80), None);
        assert_eq!(
            m.cursor_position(grid((5, 2)), 80),
            Some(DisplayPoint { row: 3, col: 2 })
        );
    }

    #[test]
    fn fold_mapping_boundaries() {
        // 可见 0..=6;折叠 2..=4 → 占位 1 行。
        let m = ViewMap::new(0, 6, &[(2, 4)]);
        assert_eq!(m.entries.len(), 5, "0, 1, 占位(2..4), 5, 6 → 5 行");
        // 反向。
        assert_eq!(m.row_to_grid(0), Some(0));
        assert_eq!(m.row_to_grid(1), Some(1));
        assert_eq!(m.row_to_grid(2), Some(2), "占位行返回折叠首行");
        assert_eq!(m.row_to_grid(3), Some(5));
        assert_eq!(m.row_to_grid(4), Some(6));
        // 正向。
        assert_eq!(m.grid_to_row(0), Some(0));
        assert_eq!(m.grid_to_row(1), Some(1));
        assert_eq!(m.grid_to_row(2), Some(2));
        assert_eq!(m.grid_to_row(3), Some(2), "折叠区间内 → 占位行");
        assert_eq!(m.grid_to_row(4), Some(2));
        assert_eq!(m.grid_to_row(5), Some(3), "折叠后的行映射到自己(边界修复)");
        assert_eq!(m.grid_to_row(6), Some(4));
        // 范围外。
        assert_eq!(m.grid_to_row(7), None);
        assert_eq!(m.grid_to_row(-1), None);
    }

    /// 无折叠:恒等映射。
    #[test]
    fn identity_without_folds() {
        let m = ViewMap::new(-3, 2, &[]);
        assert_eq!(m.entries.len(), 6);
        assert_eq!(m.grid_to_row(-3), Some(0));
        assert_eq!(m.grid_to_row(2), Some(5));
        assert_eq!(m.row_to_grid(5), Some(2));
        assert!(!m.entries.iter().any(|e| e.marker));
    }

    /// 投影:折叠感知 + 列钳制。
    #[test]
    fn projection_respects_folds_and_clamps() {
        let m = ViewMap::new(0, 6, &[(2, 4)]);
        assert_eq!(m.project(grid((3, 2)), 10), DisplayPoint { row: 2, col: 2 });
        assert_eq!(
            m.project(grid((6, 99)), 10),
            DisplayPoint { row: 4, col: 9 },
            "列钳到 cols-1"
        );
        // 视口外 → 钳到最后一行(索引 4 = 网格行 6)。
        assert_eq!(m.project(grid((9, 0)), 10), DisplayPoint { row: 4, col: 0 });
    }

    /// 鼠标选区投影:行序归一 + 反向拖选列序归一 + 折叠感知。
    #[test]
    fn mouse_selection_normalizes_order_and_respects_folds() {
        let m = ViewMap::new(0, 6, &[(2, 4)]);
        let meta = SessionMeta::default();
        // 行 1 → 显示行 1;行 5 → 显示行 3(折叠占位行 2)。
        let expected = Some(tay_render::SelectionRange {
            start: (2, 1),
            end: (3, 3),
        });
        assert_eq!(
            selection_to_display(
                Some(((HistoryAnchor(1), 2), (HistoryAnchor(5), 3))),
                None,
                meta,
                &m,
                10
            ),
            expected
        );
        // 反向拖选(起点在终点之后)→ 同一结果。
        assert_eq!(
            selection_to_display(
                Some(((HistoryAnchor(5), 3), (HistoryAnchor(1), 2))),
                None,
                meta,
                &m,
                10
            ),
            expected
        );
    }

    /// vi 选区投影:会话侧点 → 显示点,折叠区间内落到占位行。
    #[test]
    fn vi_selection_projects_through_folds() {
        use alacritty_terminal::index::{Column, Line, Point};
        use alacritty_terminal::selection::SelectionRange;
        let m = ViewMap::new(0, 6, &[(2, 4)]);
        let sel = selection_to_display(
            None,
            Some(SelectionRange {
                start: Point::new(Line(3), Column(2)),
                end: Point::new(Line(5), Column(4)),
                is_block: false,
            }),
            SessionMeta::default(),
            &m,
            10,
        );
        assert_eq!(
            sel,
            Some(tay_render::SelectionRange {
                start: (2, 2),
                end: (4, 3),
            })
        );
    }
}
