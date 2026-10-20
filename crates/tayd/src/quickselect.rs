//! QuickSelect(DESIGN §8.3):为 URL / 路径 / git hash 打标签,按键复制或打开。
//!
//! 对标 WezTerm QuickSelect:一次触发,给每个匹配项打上 1-2 字符标签,
//! 按标签即完成复制(URL 同时 xdg-open)。

use regex::Regex;

use tay_render::Grid;

/// 一个匹配项(显示坐标)。
#[derive(Clone, Debug)]
pub struct Match {
    pub row: usize,
    pub col: usize,
    pub text: String,
    pub is_url: bool,
    pub label: String,
}

/// 从可见网格提取匹配项并分配标签。
pub fn build(grid: &Grid) -> Vec<Match> {
    // regex 使用字节偏移；overlay 使用终端显示列，跳过宽字符占位格。
    let row_texts: Vec<(String, Vec<usize>)> = (0..grid.rows)
        .map(|r| {
            let mut text = String::new();
            let mut col_of_byte: Vec<usize> = Vec::new();
            for i in 0..grid.cols {
                let cell = &grid.cells[r * grid.cols + i];
                if cell.wide_spacer {
                    continue; // 宽字符占位格:本身无字符。
                }
                col_of_byte.extend(std::iter::repeat_n(i, cell.c.len_utf8()));
                text.push(cell.c);
                if let Some(extra) = &cell.zerowidth {
                    col_of_byte.extend(std::iter::repeat_n(i, extra.len()));
                    text.push_str(extra);
                }
            }
            (text, col_of_byte)
        })
        .collect();

    let url = Regex::new("https?://[^\\s<>'\"()]+").unwrap();
    let sha = Regex::new("\\b[0-9a-f]{7,40}\\b").unwrap();
    let path = Regex::new("(?:/|~/|\\./|\\.\\./)[^\\s<>'\"():;|]+").unwrap();

    let mut matches = Vec::new();
    for (row, (text, col_of_char)) in row_texts.iter().enumerate() {
        // 同一行的三类匹配,按列号收集后排序去重(URL 优先于 path 重叠)。
        let mut row_matches: Vec<(usize, usize, bool)> = Vec::new();
        for m in url.find_iter(text) {
            row_matches.push((m.start(), m.end() - m.start(), true));
        }
        for m in sha.find_iter(text) {
            row_matches.push((m.start(), m.end() - m.start(), false));
        }
        for m in path.find_iter(text) {
            row_matches.push((m.start(), m.end() - m.start(), false));
        }
        row_matches.sort_by_key(|(s, l, _)| (*s, *l));
        // 去重叠:保留靠前/更长的。
        let mut kept: Vec<(usize, usize, bool)> = Vec::new();
        for m in row_matches {
            if kept.iter().any(|(s, l, _)| m.0 < s + l && m.0 + m.1 > *s) {
                continue;
            }
            kept.push(m);
        }
        for (start, len, is_url) in kept {
            matches.push(Match {
                row,
                // 字节偏移映射回包含宽字符占位格的终端列。
                col: col_of_char[start],
                text: text[start..start + len].to_string(),
                is_url,
                label: String::new(),
            });
        }
    }

    // 标签:a-z,aa-zz…
    let labels = (0..matches.len()).map(label_for);
    for (m, label) in matches.iter_mut().zip(labels) {
        m.label = label;
    }
    matches
}

fn label_for(i: usize) -> String {
    let letters = b"abcdefghijklmnopqrstuvwxyz";
    if i < 26 {
        (letters[i] as char).to_string()
    } else {
        let i = i - 26;
        let (a, b) = (i / 26, i % 26);
        format!("{}{}", letters[a.min(25)] as char, letters[b] as char)
    }
}

/// 触发动作:复制到剪贴板;`open` 且为 URL 时同时 xdg-open。
/// 打开动作与复制分离 —— 普通标签只复制,Shift+标签才打开,
/// 避免误触标签即开网页。
/// `clipboard` 必须是常驻实例(Wayland 剪贴板数据归属存活连接,
/// 每次新建即弃可能丢数据)。
pub fn activate(m: &Match, open: bool, clipboard: &mut Option<arboard::Clipboard>) {
    if let Some(cb) = clipboard {
        let _ = cb.set_text(m.text.clone());
    }
    if open && m.is_url {
        let _ = std::process::Command::new("xdg-open").arg(&m.text).spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid_with(text: &str) -> Grid {
        let cols = 80;
        let mut cells = vec![
            tay_render::Cell {
                c: ' ',
                fg: tay_config::Rgb::new(0xd4, 0xd4, 0xd4),
                bg: tay_config::Rgb::new(0x0f, 0x11, 0x14),
                bold: false,
                italic: false,
                underline: false,
                wide_spacer: false,
                zerowidth: None,
            };
            cols
        ];
        for (i, ch) in text.chars().take(cols).enumerate() {
            cells[i].c = ch;
        }
        Grid {
            cols,
            rows: 1,
            cells,
            cursor: (0, 0),
            cursor_shape: tay_render::CursorShape::Block,
            selection: None,
            preedit: None,
            badges: Vec::new(),
            overlays: Vec::new(),
            images: Vec::new(),
            window_icons: vec![],
            chrome_panels: vec![],
            tab_bar_rows: 0,
            scrollbar: None,
        }
    }

    #[test]
    fn combining_characters_are_preserved_in_paths() {
        let mut grid = grid_with("./cafe.txt https://a.b");
        grid.cells[5].zerowidth = Some(std::sync::Arc::from("\u{301}"));
        let matches = build(&grid);
        assert_eq!(matches[0].text, "./cafe\u{301}.txt");
        assert_eq!(matches[1].col, 11);
    }
    #[test]
    fn finds_url_and_sha_and_path() {
        let g = grid_with("see https://example.com/x and deadbeef123 and /etc/passwd ok");
        let m = build(&g);
        assert_eq!(m.len(), 3);
        assert!(m[0].is_url);
        assert_eq!(m[0].text, "https://example.com/x");
        assert_eq!(m[0].label, "a");
        assert_eq!(m[1].text, "deadbeef123");
        assert_eq!(m[2].text, "/etc/passwd");
    }

    #[test]
    fn labels_are_unique() {
        let g = grid_with("https://a.com https://b.com https://c.com https://d.com https://e.com");
        let m = build(&g);
        let mut labels: Vec<_> = m.iter().map(|x| x.label.clone()).collect();
        labels.sort();
        labels.dedup();
        assert_eq!(labels.len(), m.len());
    }

    /// P2e 回归:多字节字符(宽字符)前的匹配项,列号必须按字节→字符
    /// 换算 —— 渲染器 overlay 按字符序号替换行内字符,cosmic-text
    /// 排版让宽字符占 2 列,标签像素位置自动正确。
    #[test]
    fn wide_char_shifts_match_column() {
        let cols = 14;
        let blank = tay_render::Cell {
            c: ' ',
            fg: tay_config::Rgb::new(0xd4, 0xd4, 0xd4),
            bg: tay_config::Rgb::new(0x0f, 0x11, 0x14),
            bold: false,
            italic: false,
            underline: false,
            wide_spacer: false,
            zerowidth: None,
        };
        let mut cells = vec![blank; cols];
        cells[0].c = '你';
        cells[1].wide_spacer = true; // 宽字符占位格。
        for (i, ch) in " https://a.b".chars().enumerate() {
            cells[2 + i].c = ch;
        }
        let g = Grid {
            cols,
            rows: 1,
            cells,
            cursor: (0, 0),
            cursor_shape: tay_render::CursorShape::Block,
            selection: None,
            preedit: None,
            badges: Vec::new(),
            overlays: Vec::new(),
            images: Vec::new(),
            window_icons: vec![],
            chrome_panels: vec![],
            tab_bar_rows: 0,
            scrollbar: None,
        };
        let m = build(&g);
        assert_eq!(m.len(), 1);
        // A wide character occupies two terminal columns: h starts at column 3.
        assert_eq!(m[0].col, 3);
        assert_eq!(m[0].text, "https://a.b");
    }
}
