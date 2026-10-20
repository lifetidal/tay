//! 键盘 → PTY 字节翻译。
//!
//! alacritty_terminal 0.26 起键盘绑定由前端负责:这里实现 xterm 风格的
//! 常用键位(fish/bash 日常使用的子集),kitty 协议等 P1 项见 DESIGN §7。

use tay_window::{KeyKind, KeyPress, Mods, NamedKey};

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
            n => format!(
                "\x1b[{code}~",
                code = [15u8, 17, 18, 19, 20, 21, 23, 24][(n - 5) as usize]
            ),
        }
    } else {
        let code = if f <= 4 {
            10 + f
        } else {
            [15u8, 17, 18, 19, 20, 21, 23, 24][(f - 5) as usize]
        };
        if f <= 4 {
            format!("\x1b[1;{m}{}", (b'P' + f - 1) as char)
        } else {
            format!("\x1b[{code};{m}~")
        }
    };
    seq.into_bytes()
}

/// 控制字节:Ctrl+C → 0x03 等(ASCII 字母/符号)。
fn ctrl_byte(c: u8) -> u8 {
    match c {
        b'@'..=b'_' => c & 0x1f,
        b'`'..=b'~' => c - 0x60,
        b'?' | b'8' => 0x7f,
        b' ' | b'2' => 0,
        b'3'..=b'7' => c - b'3' + 0x1b,
        _ => c,
    }
}

fn translate_named(n: NamedKey, mods: Mods, app_cursor: bool) -> Option<Vec<u8>> {
    let v = match n {
        NamedKey::Enter => b"\r".to_vec(),
        NamedKey::Backspace => vec![0x7f],
        NamedKey::Tab if mods.shift => b"\x1b[Z".to_vec(),
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
        NamedKey::F(f) => return Some(fkey_seq(f, mods)),
    };
    Some(v)
}

/// 翻译按键。`None` 表示该键已由调用方特殊处理(剪贴板等)。
pub fn translate(k: &KeyPress, mods: Mods, app_cursor: bool) -> Option<Vec<u8>> {
    if k.key == KeyKind::Char('\0') && k.text.is_none() {
        return None;
    }
    let ctrl = mods.ctrl;
    let alt = mods.alt;
    let text = k.text.as_deref();

    // F 键(不受 ctrl 影响之前先处理)。
    if let KeyKind::Named(NamedKey::F(f)) = k.key {
        return Some(fkey_seq(f, mods));
    }

    let is_named_escape = |n: NamedKey| {
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

    // Alt+常规键:xterm 的 ESC 前缀(fish 的 alt 绑定依赖)。
    if alt && !ctrl {
        if let KeyKind::Named(n) = k.key
            && !matches!(
                n,
                NamedKey::Enter | NamedKey::Backspace | NamedKey::Tab | NamedKey::Escape
            )
        {
            return translate_named(n, mods, app_cursor);
        }
        let base = match k.key {
            KeyKind::Named(n) if is_named_escape(n) => translate_named(n, mods, app_cursor)?,
            KeyKind::Char(c) => text
                .map(|t| t.as_bytes().to_vec())
                .unwrap_or_else(|| c.to_string().into_bytes()),
            KeyKind::Named(n) => return translate_named(n, mods, app_cursor),
        };
        let mut v = vec![0x1b];
        v.extend_from_slice(&base);
        return Some(v);
    }

    // Ctrl+字符 → 控制字节。
    if ctrl {
        if let KeyKind::Named(n) = k.key {
            return if n == NamedKey::Enter {
                Some(vec![b'\n'])
            } else {
                translate_named(n, mods, app_cursor)
            };
        }
        let c = match k.key {
            KeyKind::Char(c) => Some(c),
            _ => None,
        };
        if let Some(c) = c
            && c.is_ascii()
        {
            let mut bytes = if alt { vec![0x1b] } else { vec![] };
            bytes.push(ctrl_byte(c as u8));
            return Some(bytes);
        }
        return None;
    }

    match k.key {
        KeyKind::Named(n) => translate_named(n, mods, app_cursor),
        KeyKind::Char(c) => text
            .map(|t| t.as_bytes().to_vec())
            .or_else(|| Some(c.to_string().into_bytes())),
    }
}

/// kitty keyboard protocol 渐进增强标志(TermMode 位)。
#[derive(Clone, Copy, Debug, Default)]
pub struct KittyFlags {
    /// 基础协议(CSI u 消歧)。
    pub disambiguate: bool,
    /// 上报按下/抬起/重复事件类型。
    pub report_events: bool,
    /// alt 键也走 CSI u(而非 ESC 前缀)。
    pub report_alternate: bool,
    /// 所有键都走 CSI u(含纯文本)。
    pub report_all: bool,
    /// 附带关联文本。
    pub report_associated: bool,
}

/// 事件类型(flag 2)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyEventType {
    Press,
    Repeat,
    Release,
}

fn kitty_mods(mods: Mods) -> u32 {
    1 + mods.shift as u32 + mods.alt as u32 * 2 + mods.ctrl as u32 * 4 + mods.meta as u32 * 8
}

fn kitty_seq(
    code: u32,
    mods: u32,
    event: Option<KeyEventType>,
    associated: Option<&str>,
) -> Vec<u8> {
    let mut s = format!("\x1b[{code}");
    if mods > 1 {
        s.push_str(&format!(";{mods}"));
    }
    if let Some(ev) = event {
        let t = match ev {
            KeyEventType::Press => 1,
            KeyEventType::Repeat => 2,
            KeyEventType::Release => 3,
        };
        s.push_str(&format!(":{t}"));
    }
    if let Some(text) = associated {
        s.push(';');
        s.push_str(text);
    }
    s.push('u');
    s.into_bytes()
}

/// kitty keyboard protocol 编码(kitty 官方语义)。
pub fn translate_kitty(
    k: &KeyPress,
    mods: Mods,
    flags: &KittyFlags,
    event: KeyEventType,
) -> Option<Vec<u8>> {
    if k.key == KeyKind::Char('\0') && k.text.is_none() {
        return None;
    }
    let text = k.text.as_deref();
    let associated = flags.report_associated.then_some(text).flatten();
    let ev_suffix = flags.report_events.then_some(event);

    // 特殊键映射(协议规定:Enter→13、Tab→9、Esc→27、Backspace→127)。
    let named_code = |n: NamedKey| -> u32 {
        match n {
            NamedKey::Enter => 13,
            NamedKey::Tab => 9,
            NamedKey::Escape => 27,
            NamedKey::Backspace => 127,
            _ => k.raw_code,
        }
    };

    match k.key {
        KeyKind::Named(n) => Some(kitty_seq(
            named_code(n),
            kitty_mods(mods),
            ev_suffix,
            associated,
        )),
        KeyKind::Char(c) => {
            let ctrl = mods.ctrl;
            let alt = mods.alt;
            let shift = mods.shift;
            if ctrl || alt {
                // ctrl+可打印 → 控制码(ctrl 烘焙进码,mods 不含 ctrl 位);
                // alt 组合按 flag 4 决定形式。
                if alt && !ctrl && !flags.report_alternate {
                    // ESC + 文本。
                    let c_str = c.to_string();
                    let t = text.unwrap_or(&c_str);
                    let mut v = vec![0x1b];
                    v.extend_from_slice(t.as_bytes());
                    return Some(v);
                }
                let code = if ctrl { ctrl_key_code(c) } else { c as u32 };
                let m = if ctrl {
                    1 + mods.shift as u32 + mods.alt as u32 * 2 + mods.meta as u32 * 8
                } else {
                    kitty_mods(mods)
                };
                Some(kitty_seq(code, m, ev_suffix, associated))
            } else if shift {
                // shift+可打印 → 码点即含 shift 语义,mods 省略。
                let cp = text
                    .and_then(|t| t.chars().next())
                    .map(|t| t as u32)
                    .unwrap_or(c as u32);
                Some(kitty_seq(cp, 1, ev_suffix, associated))
            } else if flags.report_all {
                Some(kitty_seq(c as u32, 1, ev_suffix, associated))
            } else if event != KeyEventType::Release {
                // 纯文本原样输出(抬起事件只在 flag 2 时到达这里)。
                text.map(|t| t.as_bytes().to_vec())
            } else {
                None
            }
        }
    }
}

/// ctrl+字母/符号 → 控制码(与 xterm 路径一致)。
fn ctrl_key_code(c: char) -> u32 {
    if c.is_ascii() {
        ctrl_byte(c as u8) as u32
    } else {
        c as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(key: KeyKind, text: Option<&str>) -> KeyPress {
        KeyPress {
            key,
            text: text.map(|s| s.to_string()),
            repeat: false,
            raw_code: 0,
        }
    }

    fn char_key(c: char) -> KeyKind {
        KeyKind::Char(c)
    }

    #[test]
    fn alt_named_keys_have_one_csi_prefix() {
        let alt = Mods {
            alt: true,
            ..Mods::default()
        };
        let key = press(KeyKind::Named(NamedKey::ArrowLeft), None);
        assert_eq!(translate(&key, alt, false).unwrap(), b"\x1b[1;3D");
        let ctrl_alt = Mods { ctrl: true, ..alt };
        assert_eq!(
            translate(&press(KeyKind::Char('c'), Some("\x03")), ctrl_alt, false).unwrap(),
            b"\x1b\x03"
        );
    }

    #[test]
    fn modifiers_do_not_emit_nul() {
        let key = press(KeyKind::Char('\0'), None);
        assert_eq!(translate(&key, Mods::default(), false), None);
        assert_eq!(
            translate(
                &key,
                Mods {
                    ctrl: true,
                    ..Mods::default()
                },
                false
            ),
            None
        );
    }

    #[test]
    fn terminal_modifier_sequences() {
        let ctrl = Mods {
            ctrl: true,
            ..Mods::default()
        };
        assert_eq!(
            translate_named(NamedKey::ArrowLeft, ctrl, false).unwrap(),
            b"\x1b[1;5D"
        );
        let key = KeyPress {
            key: KeyKind::Named(NamedKey::ArrowLeft),
            text: None,
            repeat: false,
            raw_code: 0,
        };
        assert_eq!(translate(&key, ctrl, false).unwrap(), b"\x1b[1;5D");
        let shift = Mods {
            shift: true,
            ..Mods::default()
        };
        assert_eq!(
            translate_named(NamedKey::Tab, shift, false).unwrap(),
            b"\x1b[Z"
        );
        assert_eq!(fkey_seq(1, ctrl), b"\x1b[1;5P");
        assert_eq!(fkey_seq(5, ctrl), b"\x1b[15;5~");
        assert_eq!(ctrl_byte(b' '), 0);
    }

    #[test]
    fn plain_text() {
        let k = press(char_key('a'), Some("a"));
        assert_eq!(translate(&k, Mods::default(), false), Some(b"a".to_vec()));
    }

    #[test]
    fn ctrl_c() {
        let k = press(char_key('c'), Some("c"));
        assert_eq!(
            translate(
                &k,
                Mods {
                    ctrl: true,
                    ..Default::default()
                },
                false
            ),
            Some(vec![3])
        );
    }

    #[test]
    fn arrows() {
        let up = press(KeyKind::Named(NamedKey::ArrowUp), None);
        assert_eq!(
            translate(&up, Mods::default(), false),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            translate(&up, Mods::default(), true),
            Some(b"\x1bOA".to_vec())
        );
        let shift_up = translate(
            &up,
            Mods {
                shift: true,
                ..Default::default()
            },
            false,
        );
        assert_eq!(shift_up, Some(b"\x1b[1;2A".to_vec()));
    }

    #[test]
    fn f_keys() {
        let f1 = press(KeyKind::Named(NamedKey::F(1)), None);
        assert_eq!(
            translate(&f1, Mods::default(), false),
            Some(b"\x1bOP".to_vec())
        );
        let f5 = press(KeyKind::Named(NamedKey::F(5)), None);
        assert_eq!(
            translate(&f5, Mods::default(), false),
            Some(b"\x1b[15~".to_vec())
        );
    }

    #[test]
    fn alt_prefix() {
        let k = press(char_key('b'), Some("b"));
        assert_eq!(
            translate(
                &k,
                Mods {
                    alt: true,
                    ..Default::default()
                },
                false
            ),
            Some(b"\x1bb".to_vec())
        );
    }

    #[test]
    fn kitty_basic_special_keys() {
        let f = KittyFlags {
            disambiguate: true,
            ..Default::default()
        };
        let enter = KeyPress {
            key: KeyKind::Named(NamedKey::Enter),
            text: Some("\r".into()),
            repeat: false,
            raw_code: 28,
        };
        assert_eq!(
            translate_kitty(&enter, Mods::default(), &f, KeyEventType::Press),
            Some(b"\x1b[13u".to_vec())
        );
        let up = KeyPress {
            key: KeyKind::Named(NamedKey::ArrowUp),
            text: None,
            repeat: false,
            raw_code: 103,
        };
        assert_eq!(
            translate_kitty(&up, Mods::default(), &f, KeyEventType::Press),
            Some(b"\x1b[103u".to_vec())
        );
        let ctrl_up = translate_kitty(
            &up,
            Mods {
                ctrl: true,
                ..Default::default()
            },
            &f,
            KeyEventType::Press,
        );
        assert_eq!(ctrl_up, Some(b"\x1b[103;5u".to_vec()));
    }

    #[test]
    fn kitty_text_keys() {
        let f = KittyFlags {
            disambiguate: true,
            ..Default::default()
        };
        let a = KeyPress {
            key: KeyKind::Char('a'),
            text: Some("a".into()),
            repeat: false,
            raw_code: 30,
        };
        // 纯文本原样。
        assert_eq!(
            translate_kitty(&a, Mods::default(), &f, KeyEventType::Press),
            Some(b"a".to_vec())
        );
        // shift+A → 码点。
        let shift_a = KeyPress {
            key: KeyKind::Char('A'),
            text: Some("A".into()),
            repeat: false,
            raw_code: 30,
        };
        assert_eq!(
            translate_kitty(
                &shift_a,
                Mods {
                    shift: true,
                    ..Default::default()
                },
                &f,
                KeyEventType::Press
            ),
            Some(b"\x1b[65u".to_vec())
        );
        // ctrl+a → 控制码。
        assert_eq!(
            translate_kitty(
                &a,
                Mods {
                    ctrl: true,
                    ..Default::default()
                },
                &f,
                KeyEventType::Press
            ),
            Some(b"\x1b[1u".to_vec())
        );
        // alt+a → ESC 前缀(flag 4 关闭)。
        assert_eq!(
            translate_kitty(
                &a,
                Mods {
                    alt: true,
                    ..Default::default()
                },
                &f,
                KeyEventType::Press
            ),
            Some(b"\x1ba".to_vec())
        );
        // alt+a + flag 4 → CSI u。
        let f4 = KittyFlags {
            disambiguate: true,
            report_alternate: true,
            ..Default::default()
        };
        assert_eq!(
            translate_kitty(
                &a,
                Mods {
                    alt: true,
                    ..Default::default()
                },
                &f4,
                KeyEventType::Press
            ),
            Some(b"\x1b[97;3u".to_vec())
        );
    }

    #[test]
    fn kitty_event_types() {
        let f = KittyFlags {
            disambiguate: true,
            report_events: true,
            ..Default::default()
        };
        let a = KeyPress {
            key: KeyKind::Char('a'),
            text: Some("a".into()),
            repeat: false,
            raw_code: 30,
        };
        assert_eq!(
            translate_kitty(
                &a,
                Mods {
                    ctrl: true,
                    ..Default::default()
                },
                &f,
                KeyEventType::Press
            ),
            Some(b"\x1b[1:1u".to_vec())
        );
        assert_eq!(
            translate_kitty(
                &a,
                Mods {
                    ctrl: true,
                    ..Default::default()
                },
                &f,
                KeyEventType::Release
            ),
            Some(b"\x1b[1:3u".to_vec())
        );
    }

    #[test]
    fn delete_with_mods() {
        let del = press(KeyKind::Named(NamedKey::Delete), None);
        assert_eq!(
            translate(
                &del,
                Mods {
                    shift: true,
                    ..Default::default()
                },
                false
            ),
            Some(b"\x1b[3;2~".to_vec())
        );
    }
}
