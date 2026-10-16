//! 键盘 → PTY 字节翻译。
//!
//! alacritty_terminal 0.26 起键盘绑定由前端负责:这里实现 xterm 风格的
//! 常用键位(fish/bash 日常使用的子集),kitty 协议等 P1 项见 DESIGN §7。

use tay_window::{Key, KeyPress, Mods, NamedKey};

/// 修饰键的 CSI 参数值:1 + shift=1 + alt=2 + ctrl=4。
fn mods_param(mods: Mods) -> u8 {
    1 + mods.shift as u8 + mods.alt as u8 * 2 + mods.ctrl as u8 * 4
}

/// CSI 字母型(方向/定位键):CSI 1;{mods}{letter}。
fn csi_letter(mods: Mods, c: u8) -> Vec<u8> {
    let p = mods_param(mods);
    if p > 1 {
        format!("\x1b[1;{p}{}", c as char).into_bytes()
    } else {
        format!("\x1b[{}", c as char).into_bytes()
    }
}

/// CSI 波浪型(Delete/Insert/PageUp/Down):CSI {code};{mods}~。
fn csi_tilde(mods: Mods, code: u8) -> Vec<u8> {
    let p = mods_param(mods);
    if p > 1 {
        format!("\x1b[{code};{p}~").into_bytes()
    } else {
        format!("\x1b[{code}~").into_bytes()
    }
}

/// 方向/定位键:应用光标模式用 SS3,否则 CSI。
fn ss3_or_csi(c: u8, app_cursor: bool, mods: Mods) -> Vec<u8> {
    if app_cursor && mods == Mods::default() {
        vec![0x1b, b'O', c]
    } else {
        csi_letter(mods, c)
    }
}

fn fkey_seq(f: u8, mods: Mods) -> Vec<u8> {
    let m = mods_param(mods);
    let seq = if m == 1 {
        match f {
            1 => "\x1bOP".to_string(),
            2 => "\x1bOQ".to_string(),
            3 => "\x1bOR".to_string(),
            4 => "\x1bOS".to_string(),
            n => format!("\x1b[{code}~", code = [15u8, 17, 18, 19, 20, 21, 23, 24][(n - 5) as usize]),
        }
    } else {
        let code = if f <= 4 { 10 + f } else { [15u8, 17, 18, 19, 20, 21, 23, 24][(f - 5) as usize] };
        format!("\x1b[{m};{code}~")
    };
    seq.into_bytes()
}

/// 控制字节:Ctrl+C → 0x03 等(ASCII 字母/符号)。
fn ctrl_byte(c: u8) -> u8 {
    match c {
        b'@'..=b'_' => c & 0x1f,
        b'`'..=b'~' => c - 0x60,
        b'?' => 0x7f,
        _ => c,
    }
}

/// 翻译按键。`None` 表示该键已由调用方特殊处理(剪贴板等)。
pub fn translate(k: &KeyPress, mods: Mods, app_cursor: bool) -> Option<Vec<u8>> {
    let ctrl = mods.ctrl;
    let alt = mods.alt;
    let text = k.text.as_deref();

    let is_named_escape = |n: &NamedKey| {
        matches!(
            n,
            NamedKey::Enter
                | NamedKey::Backspace
                | NamedKey::Tab
                | NamedKey::Escape
                | NamedKey::ArrowUp
                | NamedKey::ArrowDown
                | NamedKey::ArrowLeft
                | NamedKey::ArrowRight
                | NamedKey::Home
                | NamedKey::End
                | NamedKey::PageUp
                | NamedKey::PageDown
                | NamedKey::Delete
                | NamedKey::Insert
        )
    };

    // F 键。
    if let Key::Named(n) = &k.logical {
        let f = match n {
            NamedKey::F1 => Some(1),
            NamedKey::F2 => Some(2),
            NamedKey::F3 => Some(3),
            NamedKey::F4 => Some(4),
            NamedKey::F5 => Some(5),
            NamedKey::F6 => Some(6),
            NamedKey::F7 => Some(7),
            NamedKey::F8 => Some(8),
            NamedKey::F9 => Some(9),
            NamedKey::F10 => Some(10),
            NamedKey::F11 => Some(11),
            NamedKey::F12 => Some(12),
            _ => None,
        };
        if let Some(f) = f {
            return Some(fkey_seq(f, mods));
        }
    }

    // Alt+常规键:xterm 的 ESC 前缀(fish 的 alt 绑定依赖)。
    if alt && !ctrl {
        let base = match &k.logical {
            Key::Named(n) if is_named_escape(n) => match translate_named(n, mods, app_cursor) {
                Some(v) => v,
                None => return None,
            },
            _ => text.map(|t| t.as_bytes().to_vec())?,
        };
        let mut v = vec![0x1b];
        v.extend_from_slice(&base);
        return Some(v);
    }

    // Ctrl+字符 → 控制字节。
    if ctrl && !alt {
        if let Some(t) = text {
            if let Some(&b) = t.as_bytes().first() {
                if b.is_ascii() {
                    return Some(vec![ctrl_byte(b)]);
                }
            }
        }
        // 布局未给出 text 时,用逻辑键兜底。
        if let Key::Character(s) = &k.logical {
            if let Some(&b) = s.as_bytes().first() {
                if b.is_ascii() {
                    return Some(vec![ctrl_byte(b)]);
                }
            }
        }
        if matches!(k.logical, Key::Named(NamedKey::Enter)) {
            return Some(vec![b'\n']);
        }
        return None;
    }

    match &k.logical {
        Key::Named(n) => translate_named(n, mods, app_cursor),
        _ => text.map(|t| t.as_bytes().to_vec()),
    }
}

fn translate_named(n: &NamedKey, mods: Mods, app_cursor: bool) -> Option<Vec<u8>> {
    let v = match n {
        NamedKey::Enter => b"\r".to_vec(),
        NamedKey::Backspace => vec![0x7f],
        NamedKey::Tab => vec![b'\t'],
        NamedKey::Escape => vec![0x1b],
        NamedKey::Delete => csi_tilde(mods, 3),
        NamedKey::Insert => csi_tilde(mods, 2),
        NamedKey::ArrowUp => ss3_or_csi(b'A', app_cursor, mods),
        NamedKey::ArrowDown => ss3_or_csi(b'B', app_cursor, mods),
        NamedKey::ArrowRight => ss3_or_csi(b'C', app_cursor, mods),
        NamedKey::ArrowLeft => ss3_or_csi(b'D', app_cursor, mods),
        NamedKey::Home => ss3_or_csi(b'H', app_cursor, mods),
        NamedKey::End => ss3_or_csi(b'F', app_cursor, mods),
        NamedKey::PageUp => csi_tilde(mods, 5),
        NamedKey::PageDown => csi_tilde(mods, 6),
        _ => return None,
    };
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tay_window::KeyCode;

    fn press(logical: Key, text: Option<&str>, mods: Mods) -> KeyPress {
        KeyPress {
            physical: KeyCode::KeyA,
            logical,
            text: text.map(|s| s.to_string()),
            repeat: false,
        }
    }

    #[test]
    fn plain_text() {
        let k = press(Key::Character("a".into()), Some("a"), Mods::default());
        assert_eq!(translate(&k, Mods::default(), false), Some(b"a".to_vec()));
    }

    #[test]
    fn ctrl_c() {
        let k = press(Key::Character("c".into()), Some("c"), Mods::default());
        assert_eq!(translate(&k, Mods { ctrl: true, ..Default::default() }, false), Some(vec![3]));
    }

    #[test]
    fn arrows() {
        let up = press(Key::Named(NamedKey::ArrowUp), None, Mods::default());
        assert_eq!(translate(&up, Mods::default(), false), Some(b"\x1b[A".to_vec()));
        assert_eq!(translate(&up, Mods::default(), true), Some(b"\x1bOA".to_vec()));
        let shift_up = translate(&up, Mods { shift: true, ..Default::default() }, false);
        assert_eq!(shift_up, Some(b"\x1b[1;2A".to_vec()));
    }

    #[test]
    fn f_keys() {
        let f1 = press(Key::Named(NamedKey::F1), None, Mods::default());
        assert_eq!(translate(&f1, Mods::default(), false), Some(b"\x1bOP".to_vec()));
        let f5 = press(Key::Named(NamedKey::F5), None, Mods::default());
        assert_eq!(translate(&f5, Mods::default(), false), Some(b"\x1b[15~".to_vec()));
    }

    #[test]
    fn alt_prefix() {
        let k = press(Key::Character("b".into()), Some("b"), Mods::default());
        assert_eq!(
            translate(&k, Mods { alt: true, ..Default::default() }, false),
            Some(b"\x1bb".to_vec())
        );
    }
}
