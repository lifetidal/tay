//! Compose pane-local grids without letting selection, labels or images cross panes.
use crate::panes::{Axis, Divider, Rect};
use tay_config::Colors;
use tay_render::{Cell, CursorShape, Grid};
use unicode_width::UnicodeWidthChar;
fn put(
    grid: &mut Grid,
    rect: Rect,
    row: usize,
    col: usize,
    text: &str,
    color: Option<tay_config::Rgb>,
    background: bool,
) {
    if row >= rect.h {
        return;
    }
    let mut x = col;
    for c in text.chars() {
        let width = c.width().unwrap_or(0);
        if width == 0 {
            continue;
        }
        if x + width > rect.w {
            break;
        }
        let idx = (rect.y + row) * grid.cols + rect.x + x;
        let cell = &mut grid.cells[idx];
        cell.c = c;
        cell.wide_spacer = false;
        if background {
            cell.bg = tay_config::Rgb::new(0x00, 0x8b, 0x74);
        }
        cell.zerowidth = None;
        if let Some(fg) = color {
            cell.fg = fg;
            cell.bold = true;
        }
        for offset in 1..width {
            grid.cells[idx + offset].wide_spacer = true;
        }
        x += width;
    }
}
pub(crate) fn compose(
    cols: usize,
    rows: usize,
    panes: Vec<(Rect, Grid, bool)>,
    dividers: &[Divider],
    colors: &Colors,
) -> Grid {
    let blank = Cell {
        c: ' ',
        zerowidth: None,
        fg: colors.foreground,
        bg: colors.background,
        bold: false,
        italic: false,
        underline: false,
        wide_spacer: false,
    };
    let mut out = Grid {
        cols,
        rows,
        cells: vec![blank; cols * rows],
        cursor: (0, 0),
        cursor_shape: CursorShape::Hidden,
        window_icons: vec![],
        chrome_panels: vec![],
        tab_bar_rows: 0,
        scrollbar: None,
        selection: None,
        preedit: None,
        badges: vec![],
        overlays: vec![],
        images: vec![],
    };
    for (rect, mut pane, focused) in panes {
        // Old snapshots can briefly have the previous PTY size; copy only the intersection.
        for y in 0..rect.h.min(pane.rows) {
            for x in 0..rect.w.min(pane.cols) {
                if rect.x + x >= cols || rect.y + y >= rows {
                    continue;
                }
                let mut cell = pane.cells[y * pane.cols + x].clone();
                if let Some(sel) = pane.selection
                    && (y, x) >= (sel.start.1, sel.start.0)
                    && (y, x) <= (sel.end.1, sel.end.0)
                {
                    cell.bg = colors.selection;
                }
                out.cells[(rect.y + y) * cols + rect.x + x] = cell;
            }
        }
        for badge in &pane.badges {
            let width = badge
                .text
                .chars()
                .map(|c| c.width().unwrap_or(0))
                .sum::<usize>();
            put(
                &mut out,
                rect,
                badge.row,
                rect.w.saturating_sub(width),
                &badge.text,
                Some(badge.color),
                false,
            );
        }
        for overlay in &pane.overlays {
            put(
                &mut out,
                rect,
                overlay.row,
                overlay.col,
                &overlay.text,
                None,
                true,
            );
        }
        for mut img in pane.images.drain(..) {
            img.col += rect.x;
            img.row += rect.y;
            img.clip = Some((rect.x, rect.y, rect.w, rect.h));
            out.images.push(img);
        }
        if focused {
            if pane.cursor.0 < rect.w && pane.cursor.1 < rect.h {
                out.cursor = (rect.x + pane.cursor.0, rect.y + pane.cursor.1);
                out.cursor_shape = pane.cursor_shape;
            }
            if let Some(mut preedit) = pane.preedit
                && preedit.col < rect.w
                && preedit.row < rect.h
            {
                let mut width = 0;
                let mut end = 0;
                for (i, c) in preedit.text.char_indices() {
                    let w = c.width().unwrap_or(0);
                    if width + w > rect.w - preedit.col {
                        break;
                    }
                    width += w;
                    end = i + c.len_utf8();
                }
                preedit.text.truncate(end);
                preedit.cursor_bytes = preedit.cursor_bytes.map(|(a, b)| (a.min(end), b.min(end)));
                preedit.col += rect.x;
                preedit.row += rect.y;
                preedit.clip_right = Some(rect.x + rect.w);
                out.preedit = Some(preedit);
            }
            if let Some(mut bar) = pane.scrollbar {
                bar.top += rect.y as f32;
                out.scrollbar = Some(bar);
            }
        }
    }
    for line in dividers {
        for y in line.rect.y..line.rect.y + line.rect.h {
            for x in line.rect.x..line.rect.x + line.rect.w {
                if x < cols && y < rows {
                    let cell = &mut out.cells[y * cols + x];
                    cell.c = match line.axis {
                        Axis::Columns => '│',
                        Axis::Rows => '─',
                    };
                    cell.fg = colors.cursor;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    fn grid(cols: usize, rows: usize, c: char) -> Grid {
        let colors = tay_config::Colors::default();
        let mut g = compose(cols, rows, vec![], &[], &colors);
        for cell in &mut g.cells {
            cell.c = c;
        }
        g.cursor = (1, 0);
        g.cursor_shape = CursorShape::Beam;
        g
    }
    #[test]
    fn clips_selection_labels_ime_and_images_and_translates_cursor() {
        let colors = tay_config::Colors::default();
        let a = Rect {
            x: 0,
            y: 0,
            w: 8,
            h: 3,
        };
        let b = Rect {
            x: 9,
            y: 0,
            w: 8,
            h: 3,
        };
        let mut left = grid(12, 4, 'a'); // previous snapshot is wider than its new pane
        left.selection = Some(tay_render::SelectionRange {
            start: (1, 0),
            end: (10, 2),
        });
        left.overlays.push(tay_render::Overlay {
            row: 1,
            col: 7,
            text: "overflow".into(),
        });
        left.images.push(tay_render::RenderImage {
            clip: None,
            id: 1,
            col: 0,
            row: 0,
            cols: 20,
            rows: 10,
            w: 1,
            h: 1,
            z: 1,
            rgba: std::sync::Arc::from([0u8; 4]),
        });
        let mut right = grid(8, 3, 'b');
        right.preedit = Some(tay_render::Preedit {
            text: "中文XYZ".into(),
            col: 4,
            row: 1,
            cursor_bytes: Some((6, 6)),
            clip_right: None,
        });
        let output = compose(
            17,
            3,
            vec![(a, left, false), (b, right, true)],
            &[],
            &colors,
        );
        assert_eq!(output.cursor, (10, 0));
        assert_eq!(output.cells[1].bg, colors.selection);
        assert_eq!(output.cells[17 + 9].c, 'b');
        assert_eq!(output.cells[17 + 9].bg, colors.background);
        assert_eq!(output.cells[17 + 8].c, ' ');
        assert_eq!(output.images[0].clip, Some((0, 0, 8, 3)));
        let preedit = output.preedit.unwrap();
        assert_eq!((preedit.col, preedit.row), (13, 1));
        assert_eq!(preedit.text, "中文");
        assert_eq!(preedit.clip_right, Some(17));
    }
}
