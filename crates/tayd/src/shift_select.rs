//! Shift+拖选兜底(WezTerm 语义):应用开启鼠标模式时,Shift+左键
//! 旁路进本地选区,松开即复制。决策与状态纯逻辑,可无 Ctx 单测。

use tay_window::{Mods, MouseButtonIdx};

/// 按下是否旁路鼠标上报:仅 Shift+左键。
pub fn bypass_report(mods: &Mods, button: MouseButtonIdx) -> bool {
    mods.shift && button == MouseButtonIdx::Left
}

/// 一次 Shift 拖选的状态。
pub struct ShiftSelect {
    active: bool,
}

impl ShiftSelect {
    pub fn new() -> Self {
        Self { active: false }
    }

    /// 按下左键:shift 按住则激活本地选区(事件不进应用)。
    pub fn begin(&mut self, shift: bool) -> bool {
        self.active = shift;
        shift
    }

    pub fn active(&self) -> bool {
        self.active
    }

    /// 松开:清状态;返回是否应复制(仅激活且选区非空时)。
    pub fn end(&mut self, has_selection: bool) -> bool {
        let copy = self.active && has_selection;
        self.active = false;
        copy
    }

    /// 拖选被外部打断(hide/toggle/关窗等):直接失效。
    pub fn cancel(&mut self) {
        self.active = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(shift: bool) -> Mods {
        Mods {
            shift,
            ctrl: false,
            alt: false,
            meta: false,
        }
    }

    #[test]
    fn begin_activates_only_with_shift() {
        let mut s = ShiftSelect::new();
        assert!(s.begin(true));
        assert!(s.active());
        assert!(!s.begin(false));
        assert!(!s.active());
    }

    #[test]
    fn end_copies_only_active_with_selection() {
        let mut s = ShiftSelect::new();
        s.begin(true);
        assert!(s.end(true), "激活且有选区必须复制");
        assert!(!s.active(), "结束后必须清状态");

        s.begin(true);
        assert!(!s.end(false), "激活但无选区不复制");
        assert!(!s.active());

        assert!(!s.end(true), "未激活不复制");
    }

    #[test]
    fn cancel_clears_active() {
        let mut s = ShiftSelect::new();
        s.begin(true);
        s.cancel();
        assert!(!s.active());
        assert!(!s.end(true), "取消后松开不得复制");
    }

    #[test]
    fn bypass_only_shift_left() {
        let m = mods(true);
        assert!(bypass_report(&m, MouseButtonIdx::Left));
        assert!(!bypass_report(&m, MouseButtonIdx::Right));
        assert!(!bypass_report(&m, MouseButtonIdx::Middle));
        assert!(!bypass_report(&mods(false), MouseButtonIdx::Left));
    }
}
