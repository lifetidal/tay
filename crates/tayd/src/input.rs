//! InputRouter(第八轮,架构 §2.4):按键 / IME / 粘贴的路由纯决策。
//!
//! 优先级(与 app 的分发顺序一致,集中在此以便独立测试):
//! 1. QuickSelect 激活:全量消费(标签 / Esc),不进 PTY;
//! 2. 本地快捷键第一档(折叠 Ctrl+Shift+H、vi 切换 Ctrl+Shift+Space);
//! 3. vi 复制模式:动作消费(搜索 / yank / 退出),不进 PTY;
//! 4. kitty keyboard 协议(应用启用 disambiguate):CSI u 编码;
//! 5. 本地快捷键第二档(QuickSelect 触发、复制、粘贴、提示符跳转、
//!    按块复制);
//! 6. 其余按键 → 翻译为字节序列 → PTY(交给终端程序)。
//!
//! IME:`Commit` 按文本进 PTY(与粘贴同通道);`Preedit` 仅本地显示,
//! 不进 PTY。普通按键的 release 不产生 PTY 输入(kitty flag 2
//! report_events 时除外 —— 由调用方按协议发 Release 事件)。

use tay_window::{KeyKind, KeyPress, Mods, NamedKey};

/// 按键路由结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyRoute {
    /// QuickSelect 激活:本地消费(标签 / Esc)。
    QuickSelectConsume,
    /// tay 本地动作(动作名,不进 PTY)。
    Local(&'static str),
    /// vi 复制模式消费(不进 PTY)。
    ViConsume,
    /// kitty keyboard 协议编码(CSI u)。
    KittyProtocol,
    /// 交给终端程序(翻译为字节序列后进 PTY)。
    Term,
}

/// User bindings take precedence; exact modifiers preserve Ctrl+C for the shell.
pub fn configured_action(config: &tay_config::Config, key: KeyKind, mods: Mods) -> Option<&str> {
    let mask =
        mods.ctrl as u8 | (mods.shift as u8) << 1 | (mods.alt as u8) << 2 | (mods.meta as u8) << 3;
    let name = match key {
        KeyKind::Char(' ') => "Space".into(),
        KeyKind::Char(c) => c.to_string(),
        KeyKind::Named(n) => match n {
            NamedKey::F(n) => format!("F{n}"),
            NamedKey::ArrowLeft => "Left".into(),
            NamedKey::ArrowRight => "Right".into(),
            NamedKey::ArrowUp => "Up".into(),
            NamedKey::ArrowDown => "Down".into(),
            other => format!("{other:?}"),
        },
    };
    config
        .keys
        .iter()
        .find(|b| {
            b.modifier_mask() == Ok(mask)
                && tay_config::KeyBinding::canonical_key(&b.key)
                    == tay_config::KeyBinding::canonical_key(&name)
        })
        .map(|b| b.action.as_str())
}

/// Only key-down may invoke UI actions. Repeating toggles or paste is unsafe.
pub fn configured_key_action<'a>(
    config: &'a tay_config::Config,
    key: &KeyPress,
    pressed: bool,
    mods: Mods,
) -> Option<&'a str> {
    if !pressed {
        return None;
    }
    let action = configured_action(config, key.key, mods)?;
    if key.repeat
        && [
            "new-tab",
            "close-tab",
            "hide",
            "maximize",
            "settings",
            "copy",
            "paste",
            "font-reset",
        ]
        .contains(&action)
    {
        return Some("none"); // Consume the repeat instead of sending it to the shell.
    }
    Some(action)
}

/// 本地快捷键表(全部为 Ctrl+Shift):动作名唯一。
fn local_action(key: KeyKind, mods: Mods) -> Option<&'static str> {
    if !mods.ctrl || !mods.shift || mods.alt || mods.meta {
        return None;
    }
    Some(match key {
        KeyKind::Char('h') | KeyKind::Char('H') => "fold",
        KeyKind::Char(' ') => "vi-toggle",
        KeyKind::Char('f') | KeyKind::Char('F') => "quickselect",
        KeyKind::Char('c') | KeyKind::Char('C') => "copy",
        KeyKind::Char('v') | KeyKind::Char('V') => "paste",
        KeyKind::Char('o') | KeyKind::Char('O') => "copy-block",
        KeyKind::Named(NamedKey::ArrowUp) => "prev-prompt",
        KeyKind::Named(NamedKey::ArrowDown) => "next-prompt",
        _ => return None,
    })
}

/// 第一档(在 vi 模式与 kitty 协议之前拦截):折叠、vi 切换。
fn local_action_first_tier(key: KeyKind, mods: Mods) -> Option<&'static str> {
    match local_action(key, mods) {
        Some(action @ ("fold" | "vi-toggle")) => Some(action),
        _ => None,
    }
}

/// 完整路由决策(优先级见模块文档)。
pub fn classify(
    key: KeyKind,
    mods: Mods,
    quick_active: bool,
    in_vi: bool,
    kitty_active: bool,
) -> KeyRoute {
    if quick_active {
        return KeyRoute::QuickSelectConsume;
    }
    if let Some(action) = local_action_first_tier(key, mods) {
        return KeyRoute::Local(action);
    }
    if in_vi {
        return KeyRoute::ViConsume;
    }
    if kitty_active {
        return KeyRoute::KittyProtocol;
    }
    if let Some(action) = local_action(key, mods) {
        return KeyRoute::Local(action);
    }
    KeyRoute::Term
}

/// 按键是否为某本地动作(供分发处按名匹配,表单一来源)。
pub fn is_local(key: KeyKind, mods: Mods, action: &str) -> bool {
    local_action(key, mods) == Some(action)
}

/// Paste is a single budgeted input message, including the bracket markers.
/// Remove ESC so clipboard contents cannot close bracketed paste early.
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let text = text.replace('\x1b', "").replace("\r\n", "\n");
    if bracketed {
        format!("\x1b[200~{text}\x1b[201~").into_bytes()
    } else {
        text.replace('\n', "\r").into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctrl_shift() -> Mods {
        Mods {
            ctrl: true,
            shift: true,
            ..Mods::default()
        }
    }

    /// 第一档在 vi 模式之前:vi 里 Ctrl+Shift+H/Space 仍是本地动作。
    #[test]
    fn lua_bindings_match_exact_modifiers_and_preserve_shell_keys() {
        let c = tay_config::Config::from_lua(include_str!("../../../config/desktop.lua")).unwrap();
        assert_eq!(
            configured_action(&c, KeyKind::Char('T'), ctrl_shift()),
            Some("new-tab")
        );
        assert_eq!(
            configured_action(
                &c,
                KeyKind::Char('c'),
                Mods {
                    ctrl: true,
                    ..Mods::default()
                }
            ),
            None
        );
        assert_eq!(
            configured_action(
                &c,
                KeyKind::Char('3'),
                Mods {
                    alt: true,
                    ..Mods::default()
                }
            ),
            Some("tab-3")
        );
        assert_eq!(
            configured_action(
                &c,
                KeyKind::Named(NamedKey::Insert),
                Mods {
                    shift: true,
                    ..Mods::default()
                }
            ),
            Some("paste")
        );
        assert_eq!(
            configured_action(
                &c,
                KeyKind::Char('t'),
                Mods {
                    ctrl: true,
                    shift: true,
                    alt: true,
                    ..Mods::default()
                }
            ),
            None
        );
    }

    #[test]
    fn desktop_bindings_press_release_repeat_and_shell_pass_through() {
        let config =
            tay_config::Config::from_lua(include_str!("../../../config/desktop.lua")).unwrap();
        for binding in &config.keys {
            let kind = match binding.key.as_str() {
                "Tab" => KeyKind::Named(NamedKey::Tab),
                "PageDown" => KeyKind::Named(NamedKey::PageDown),
                "PageUp" => KeyKind::Named(NamedKey::PageUp),
                "Insert" => KeyKind::Named(NamedKey::Insert),
                "F12" => KeyKind::Named(NamedKey::F(12)),
                s => KeyKind::Char(s.chars().next().unwrap().to_ascii_uppercase()),
            };
            let mask = binding.modifier_mask().unwrap();
            let mods = Mods {
                ctrl: mask & 1 != 0,
                shift: mask & 2 != 0,
                alt: mask & 4 != 0,
                meta: mask & 8 != 0,
            };
            // Control text is separate from shortcut identity, as in real XKB events.
            let mut key = KeyPress {
                key: kind,
                text: Some("\u{14}".into()),
                raw_code: 20,
                repeat: false,
            };
            assert_eq!(
                configured_key_action(&config, &key, true, mods),
                Some(binding.action.as_str()),
                "{binding:?}"
            );
            assert_eq!(
                configured_key_action(&config, &key, false, mods),
                None,
                "release fired {binding:?}"
            );
            key.repeat = true;
            if [
                "new-tab",
                "close-tab",
                "hide",
                "maximize",
                "settings",
                "paste",
                "copy",
                "font-reset",
            ]
            .contains(&binding.action.as_str())
            {
                assert_eq!(
                    configured_key_action(&config, &key, true, mods),
                    Some("none")
                );
            }
        }
        for c in ['c', 'v', 'a', 'e', 'r', 'l', 'd', 'z', 'm'] {
            assert_eq!(
                configured_action(
                    &config,
                    KeyKind::Char(c),
                    Mods {
                        ctrl: true,
                        ..Mods::default()
                    }
                ),
                None,
                "shell Ctrl+{c} was swallowed"
            );
        }
        let ctrl = Mods {
            ctrl: true,
            ..Mods::default()
        };
        assert_eq!(
            configured_action(&config, KeyKind::Char('t'), ctrl),
            Some("new-tab")
        );
        assert_eq!(
            configured_action(&config, KeyKind::Char('w'), ctrl),
            Some("close-tab")
        );
    }

    #[test]
    fn extra_modifiers_do_not_trigger_legacy_local_actions() {
        for (alt, meta) in [(true, false), (false, true), (true, true)] {
            let mods = Mods {
                ctrl: true,
                shift: true,
                alt,
                meta,
            };
            assert_eq!(
                classify(KeyKind::Char('c'), mods, false, false, false),
                KeyRoute::Term
            );
            assert_eq!(
                classify(KeyKind::Char('h'), mods, false, false, false),
                KeyRoute::Term
            );
        }
    }

    #[test]
    fn multiline_paste_obeys_terminal_mode() {
        assert_eq!(paste_bytes("one\r\ntwo\n", false), b"one\rtwo\r");
        assert_eq!(paste_bytes("one\ntwo", true), b"\x1b[200~one\ntwo\x1b[201~");
        assert_eq!(paste_bytes("\x1b[201~", true), b"\x1b[200~[201~\x1b[201~");
    }

    #[test]
    fn first_tier_beats_vi_mode() {
        let m = ctrl_shift();
        assert_eq!(
            classify(KeyKind::Char('h'), m, false, true, false),
            KeyRoute::Local("fold"),
            "vi 模式下折叠快捷键不被 vi 吞掉"
        );
        assert_eq!(
            classify(KeyKind::Char(' '), m, false, true, false),
            KeyRoute::Local("vi-toggle"),
            "vi 模式下切换快捷键不被 vi 吞掉"
        );
        // 普通按键在 vi 模式被 vi 消费,不进 PTY。
        assert_eq!(
            classify(KeyKind::Char('j'), Mods::default(), false, true, false),
            KeyRoute::ViConsume
        );
    }

    /// QuickSelect 激活时全量消费。
    #[test]
    fn quickselect_consumes_everything() {
        assert_eq!(
            classify(KeyKind::Char('h'), ctrl_shift(), true, false, false),
            KeyRoute::QuickSelectConsume
        );
        assert_eq!(
            classify(KeyKind::Char('x'), Mods::default(), true, false, false),
            KeyRoute::QuickSelectConsume
        );
    }

    /// kitty 协议在第二档快捷键之前:应用启用 disambiguate 时
    /// Ctrl+Shift+F 属于应用(协议要求),但复制/粘贴由 tay 拦截。
    #[test]
    fn kitty_protocol_order_and_clipboard_shortcuts() {
        let m = ctrl_shift();
        assert_eq!(
            classify(KeyKind::Char('f'), m, false, false, true),
            KeyRoute::KittyProtocol
        );
        // 但 vi 开关在第一档,仍优先于 kitty。
        assert_eq!(
            classify(KeyKind::Char(' '), m, false, false, true),
            KeyRoute::Local("vi-toggle")
        );
        // 非 kitty 应用:本地快捷键正常。
        assert_eq!(
            classify(KeyKind::Char('f'), m, false, false, false),
            KeyRoute::Local("quickselect")
        );
    }

    /// 表完整性:全部本地动作名 + 普通按键进 PTY。
    #[test]
    fn local_table_and_term_fallback() {
        let m = ctrl_shift();
        for (k, name) in [
            (KeyKind::Char('h'), "fold"),
            (KeyKind::Char(' '), "vi-toggle"),
            (KeyKind::Char('f'), "quickselect"),
            (KeyKind::Char('c'), "copy"),
            (KeyKind::Char('v'), "paste"),
            (KeyKind::Char('o'), "copy-block"),
            (KeyKind::Named(NamedKey::ArrowUp), "prev-prompt"),
            (KeyKind::Named(NamedKey::ArrowDown), "next-prompt"),
        ] {
            assert_eq!(classify(k, m, false, false, false), KeyRoute::Local(name));
            assert!(is_local(k, m, name));
        }
        // 无修饰普通按键 → 终端。
        assert_eq!(
            classify(KeyKind::Char('a'), Mods::default(), false, false, false),
            KeyRoute::Term
        );
        // 仅 ctrl 不是本地快捷键。
        let c = Mods {
            ctrl: true,
            ..Mods::default()
        };
        assert_eq!(
            classify(KeyKind::Char('c'), c, false, false, false),
            KeyRoute::Term
        );
    }
}
