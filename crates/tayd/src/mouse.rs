//! 鼠标上报(架构 §2.4 补充):应用启用鼠标模式后,终端把指针
//! 事件编码为鼠标序列写回 PTY,本地拖选让位(kitty 同款语义)。
//!
//! 模式(应用通过 CSI ? 1000/1002/1003 h 开启):
//! - 1000 点击:按下/松开;
//! - 1002 按住拖动:按下/松开 + 按住期间的移动;
//! - 1003 任意移动:再加无按键的纯移动;
//! - 1006 SGR 编码:`ESC[<Cb;Cx;CyM/m`,坐标不受 223 限制;
//! - 无 1006 时回退 X10:`ESC[M` + 三个偏移字节(坐标钳制 223)。
//!
//! 按键码:xterm 约定 0=左,1=中,2=右,3=无按键(纯移动);
//! 移动标志 +32;滚轮 64=上,65=下,66=左,67=右;修饰位
//! 4=Shift,8=Alt/Meta,16=Ctrl。本模块是纯编码函数,可独立
//! 测试;模式判定与路由决策在 app.rs。

use tay_window::{Mods, MouseButtonIdx};

/// X10 单字节上限:线值 +32 后必须 ≤255,协议上界 223。
const X10_MAX: u32 = 223;
/// SGR 坐标上界(u16)。
const SGR_MAX: u32 = u16::MAX as u32;
/// 移动事件在按键码上的标志位。
const MOTION: u8 = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MouseEncoding {
    /// SGR(1006):坐标可携带任意大值。
    Sgr,
    /// X10:cb 与坐标均钳制到 223。
    X10,
}

/// 物理按键 → 协议按键码(0 左,1 中,2 右);未知按键不编码。
pub(crate) fn button_code(button: MouseButtonIdx) -> Option<u8> {
    match button {
        MouseButtonIdx::Left => Some(0),
        MouseButtonIdx::Middle => Some(1),
        MouseButtonIdx::Right => Some(2),
        MouseButtonIdx::Other(_) => None,
    }
}

/// xterm 修饰位:Shift=4,Alt/Meta=8,Ctrl=16。
pub(crate) fn modifier_bits(mods: &Mods) -> u8 {
    ((mods.shift as u8) * 4) | (((mods.alt || mods.meta) as u8) * 8) | ((mods.ctrl as u8) * 16)
}

/// 按下:cb = 按键码 | 修饰位,final 字母 M。
pub(crate) fn press(
    enc: MouseEncoding,
    button: u8,
    mods: &Mods,
    col: u32,
    row: u32,
) -> Vec<u8> {
    report(enc, button | modifier_bits(mods), col, row, true)
}

/// 松开:SGR 带按键码(X10 固定 3,无按键位),无移动位,
/// final 字母 m。
pub(crate) fn release(
    enc: MouseEncoding,
    button: u8,
    mods: &Mods,
    col: u32,
    row: u32,
) -> Vec<u8> {
    let cb = match enc {
        MouseEncoding::Sgr => button | modifier_bits(mods),
        MouseEncoding::X10 => 3 | modifier_bits(mods),
    };
    report(enc, cb, col, row, false)
}

/// 移动:带按键(1002 拖动)或无按键纯移动(1003);cb 加运动位。
pub(crate) fn motion(
    enc: MouseEncoding,
    button: Option<u8>,
    mods: &Mods,
    col: u32,
    row: u32,
) -> Vec<u8> {
    let cb = button.unwrap_or(3) | MOTION | modifier_bits(mods);
    report(enc, cb, col, row, true)
}

/// 滚轮:垂直 64/65、水平 66/67。SGR 发按下+松开对(应用按对
/// 计次);X10 发单次运动事件。
pub(crate) fn wheel(
    enc: MouseEncoding,
    up: bool,
    horizontal: bool,
    mods: &Mods,
    col: u32,
    row: u32,
) -> Vec<u8> {
    let cb = match (up, horizontal) {
        (true, false) => 64,
        (false, false) => 65,
        (true, true) => 66,
        (false, true) => 67,
    } | modifier_bits(mods);
    match enc {
        MouseEncoding::Sgr => {
            let mut out = report(enc, cb, col, row, true);
            out.extend(report(enc, cb, col, row, false));
            out
        }
        MouseEncoding::X10 => report(enc, cb, col, row, true),
    }
}

/// 单条鼠标序列:press = true 时 M(按下/移动),false 时 m(松开)。
fn report(enc: MouseEncoding, cb: u8, col: u32, row: u32, press: bool) -> Vec<u8> {
    match enc {
        MouseEncoding::Sgr => format!(
            "\x1b[<{};{};{}{}",
            cb,
            col.min(SGR_MAX),
            row.min(SGR_MAX),
            if press { 'M' } else { 'm' }
        )
        .into_bytes(),
        MouseEncoding::X10 => vec![
            0x1b,
            b'[',
            b'M',
            (u32::from(cb)).min(X10_MAX) as u8 + 32,
            col.min(X10_MAX) as u8 + 32,
            row.min(X10_MAX) as u8 + 32,
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_mods() -> Mods {
        Mods::default()
    }

    #[test]
    fn button_code_maps_left_middle_right_and_skips_others() {
        assert_eq!(button_code(MouseButtonIdx::Left), Some(0));
        assert_eq!(button_code(MouseButtonIdx::Middle), Some(1));
        assert_eq!(button_code(MouseButtonIdx::Right), Some(2));
        assert_eq!(button_code(MouseButtonIdx::Other(8)), None);
    }

    #[test]
    fn sgr_press_and_release_forms() {
        assert_eq!(
            press(MouseEncoding::Sgr, 0, &no_mods(), 1, 1),
            b"\x1b[<0;1;1M"
        );
        assert_eq!(
            release(MouseEncoding::Sgr, 2, &no_mods(), 10, 5),
            b"\x1b[<2;10;5m"
        );
    }

    #[test]
    fn sgr_modifier_bits_follow_xterm_convention() {
        let mods = Mods {
            ctrl: true,
            shift: true,
            alt: true,
            meta: false,
        };
        // 16(Ctrl) + 4(Shift) + 8(Alt) = 28。
        assert_eq!(press(MouseEncoding::Sgr, 0, &mods, 1, 1), b"\x1b[<28;1;1M");
        let meta = Mods {
            alt: false,
            meta: true,
            ..mods
        };
        assert_eq!(press(MouseEncoding::Sgr, 0, &meta, 1, 1), b"\x1b[<28;1;1M");
    }

    #[test]
    fn sgr_motion_adds_32_and_pure_motion_uses_button_3() {
        assert_eq!(
            motion(MouseEncoding::Sgr, Some(0), &no_mods(), 3, 4),
            b"\x1b[<32;3;4M"
        );
        assert_eq!(
            motion(MouseEncoding::Sgr, None, &no_mods(), 3, 4),
            b"\x1b[<35;3;4M"
        );
    }

    #[test]
    fn sgr_wheel_is_a_press_release_pair() {
        assert_eq!(
            wheel(MouseEncoding::Sgr, true, false, &no_mods(), 7, 2),
            b"\x1b[<64;7;2M\x1b[<64;7;2m"
        );
        assert_eq!(
            wheel(MouseEncoding::Sgr, false, false, &no_mods(), 7, 2),
            b"\x1b[<65;7;2M\x1b[<65;7;2m"
        );
    }

    #[test]
    fn sgr_coordinates_pass_through_beyond_x10_limit() {
        // 1006 无 223 限制(经典 X10 缺口)。
        assert_eq!(
            press(MouseEncoding::Sgr, 0, &no_mods(), 300, 400),
            b"\x1b[<0;300;400M"
        );
    }

    #[test]
    fn x10_offset_encoding_and_release() {
        assert_eq!(
            press(MouseEncoding::X10, 0, &no_mods(), 1, 1),
            b"\x1b[M\x20\x21\x21"
        );
        assert_eq!(
            release(MouseEncoding::X10, 0, &no_mods(), 1, 1),
            b"\x1b[M#\x21\x21"
        );
    }

    #[test]
    fn x10_clamps_coordinates_and_cb_to_223() {
        // 500 → 223;字节 = 223 + 32 = 255(单字节上限)。
        assert_eq!(
            press(MouseEncoding::X10, 0, &no_mods(), 500, 9000),
            b"\x1b[M\x20\xff\xff"
        );
    }

    #[test]
    fn x10_wheel_is_a_single_event() {
        assert_eq!(
            wheel(MouseEncoding::X10, true, false, &no_mods(), 1, 1),
            b"\x1b[M\x60\x21\x21"
        );
    }
}
