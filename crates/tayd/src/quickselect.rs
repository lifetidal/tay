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
    // 逐行文本(宽字符占位格已跳过)。
    let rows: Vec<String> = (0..grid.rows)
        .map(|r| {
            grid.cells[r * grid.cols..(r + 1) * grid.cols]
                .iter()
                .filter(|c| !c.wide_spacer)
                .map(|c| c.c)
                .collect()
        })
        .collect();

    let url = Regex::new("https?://[^\\s<>'\"()]+").unwrap();
    let sha = Regex::new("\\b[0-9a-f]{7,40}\\b").unwrap();
    let path = Regex::new("(?:/|~/|\\./|\\.\\./)[^\\s<>'\"():;|]+").unwrap();

    let mut matches = Vec::new();
    for (row, text) in rows.iter().enumerate() {
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
            // 列号 = 字符序号(M3:匹配项本身为 ASCII,不受宽字符影响)。
            matches.push(Match {
                row,
                col: start,
                text: text[start..start + len].to_string(),
                is_url,
                label: String::new(),
            });
        }
    }

    // 标签:a-z,aa-zz…
    let labels = (0..matches.len()).map(|i| label_for(i));
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

/// 触发的动作:复制到剪贴板(URL 额外 xdg-open)。
pub fn activate(m: &Match) {
    log::info!("QuickSelect: {}{}", m.text, if m.is_url { " (打开)" } else { "" });
    if let Ok(mut cb) = arboard::Clipboard::new() {
        let _ = cb.set_text(m.text.clone());
    }
    if m.is_url {
        let _ = std::process::Command::new("xdg-open")
            .arg(&m.text)
            .spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid_with(text: &str) -> Grid {
        let cols = 80;
        let mut cells = vec![tay_render::Cell {
            c: ' ',
            fg: tay_config::Rgb::new(0xd4, 0xd4, 0xd4),
            bg: tay_config::Rgb::new(0x0f, 0x11, 0x14),
            bold: false,
            italic: false,
            underline: false,
            wide_spacer: false,
        }; cols];
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
        }
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
}
