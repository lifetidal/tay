//! 呼出/收起状态机(P3c:从 TayApp 拆出)。
//!
//! 只持有状态与纯逻辑:margin 提交、deadline 设置等副作用由调用方
//! (TayApp)负责,便于单测。margin 动画规则(DESIGN §6.2):
//! - 呼出 = top margin 0,收起 = -layer_h(移出屏幕上沿);
//! - 动画中反向(toggle)从**当前实际 margin** 出发,而非动画起点;
//! - animation_ms = 0 直接到位,无动画。

use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vis {
    Hidden,
    Shown,
    /// margin 从 `from` 动画到 `to`;target = 最终呼出?
    Animating {
        from: i32,
        to: i32,
        start: Instant,
        target: bool,
    },
}

fn ease_out_cubic(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

/// 呼出状态机。
pub struct VisibilityController {
    vis: Vis,
    /// 当前实际 margin(动画每步更新;反向动画从它出发)。
    margin: i32,
    layer_h: i32,
    /// 临时固定:暂停失焦自动隐藏(`tay pin` 切换)。
    pinned: bool,
}

/// 一次状态转换的结果。
pub enum VisOutcome {
    /// 无变化(已处于目标状态)。
    None,
    /// 无动画直达:提交此 margin。
    Margin(i32),
    /// 已进入动画;showing = 动画方向是呼出?
    Animating { showing: bool },
}

impl VisibilityController {
    pub fn new(layer_h: i32) -> Self {
        Self {
            vis: Vis::Hidden,
            margin: -layer_h,
            layer_h,
            pinned: false,
        }
    }

    pub fn is_hidden(&self) -> bool {
        self.vis == Vis::Hidden
    }

    /// 当前实际 margin(输出回退重建 layer 后由应用重新提交)。
    pub fn margin(&self) -> i32 {
        self.margin
    }

    /// 是否完全呼出(测试用)。
    #[allow(dead_code)]
    pub fn is_shown(&self) -> bool {
        self.vis == Vis::Shown
    }

    pub fn animating(&self) -> bool {
        matches!(self.vis, Vis::Animating { .. })
    }

    pub fn pinned(&self) -> bool {
        self.pinned
    }

    pub fn toggle_pin(&mut self) -> bool {
        self.pinned = !self.pinned;
        self.pinned
    }

    /// layer 高度变化(输出切换/尺寸重算):隐藏时重锚 margin。
    /// layer 高度变化(输出切换/尺寸重算):隐藏时重锚 margin;收起
    /// 方向(to = -旧高度)的动画同步改目标;margin 不得比新 -h 更深。
    pub fn set_layer_h(&mut self, h: i32) {
        let old = self.layer_h;
        self.layer_h = h;
        match &mut self.vis {
            Vis::Hidden => self.margin = -h,
            Vis::Animating { to, .. } if *to == -old => {
                *to = -h;
                self.margin = self.margin.max(-h);
            }
            _ => {
                self.margin = self.margin.max(-h);
            }
        }
    }

    /// 呼出。
    pub fn show(&mut self, animation_ms: u32) -> VisOutcome {
        if self.vis == Vis::Shown {
            return VisOutcome::None;
        }
        if animation_ms == 0 {
            // 无动画:直接到位(P2c)。
            self.vis = Vis::Shown;
            return if self.margin != 0 {
                self.margin = 0;
                VisOutcome::Margin(0)
            } else {
                VisOutcome::Animating { showing: true }
            };
        }
        let from = self.margin;
        self.vis = Vis::Animating {
            from,
            to: 0,
            start: Instant::now(),
            target: true,
        };
        VisOutcome::Animating { showing: true }
    }

    /// 收起。
    pub fn hide(&mut self, animation_ms: u32) -> VisOutcome {
        if self.vis == Vis::Hidden {
            return VisOutcome::None;
        }
        if animation_ms == 0 {
            self.vis = Vis::Hidden;
            return if self.margin != -self.layer_h {
                self.margin = -self.layer_h;
                VisOutcome::Margin(-self.layer_h)
            } else {
                VisOutcome::Animating { showing: false }
            };
        }
        let from = self.margin;
        self.vis = Vis::Animating {
            from,
            to: -self.layer_h,
            start: Instant::now(),
            target: false,
        };
        VisOutcome::Animating { showing: false }
    }

    /// toggle:动画中则反向(从当前实际位置,而非动画起点)。
    pub fn toggle(&mut self, animation_ms: u32) -> VisOutcome {
        match self.vis {
            Vis::Hidden => self.show(animation_ms),
            Vis::Shown => self.hide(animation_ms),
            Vis::Animating { target, .. } => {
                if target {
                    self.hide(animation_ms)
                } else {
                    self.show(animation_ms)
                }
            }
        }
    }

    /// 动画步进:返回要提交的新 margin(无变化 = None)。
    pub fn step(&mut self, animation_ms: u32) -> Option<i32> {
        self.step_at(Instant::now(), animation_ms)
    }

    /// 动画步进(注入时钟,测试确定性用)。
    /// 返回本次需要提交的新 margin;**返回 None 只代表"本次无需提交",
    /// 不代表动画结束** —— 到期收敛(进入 Shown/Hidden)与 margin 提交
    /// 是两件独立的事:取整可能让 margin 提前到达终点,此时必须靠
    /// 时钟收敛,否则 vis 永远卡在 Animating。
    pub fn step_at(&mut self, now: Instant, animation_ms: u32) -> Option<i32> {
        let Vis::Animating {
            from,
            to,
            start,
            target,
        } = self.vis
        else {
            return None;
        };
        let ms = animation_ms.max(1) as f32;
        let t = ((now - start).as_secs_f32() * 1000.0 / ms).clamp(0.0, 1.0);
        let margin = from + ((to - from) as f32 * ease_out_cubic(t)).round() as i32;
        let changed = margin != self.margin;
        if changed {
            self.margin = margin;
        }
        // 到期必须收敛,即使 margin 因取整早已到达终点而"无变化"。
        if t >= 1.0 {
            self.margin = to;
            self.vis = if target { Vis::Shown } else { Vis::Hidden };
        }
        changed.then_some(self.margin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn show_hide_instant_with_zero_animation() {
        let mut v = VisibilityController::new(400);
        assert!(v.is_hidden());
        assert!(matches!(v.show(0), VisOutcome::Margin(0)));
        assert!(v.is_shown());
        assert!(matches!(v.hide(0), VisOutcome::Margin(-400)));
        assert!(v.is_hidden());
    }

    #[test]
    fn reversal_starts_from_current_position() {
        let mut v = VisibilityController::new(400);
        let t0 = Instant::now();
        v.show(150);
        // 动画进行到一半(注入时钟)。
        let mid_t = t0 + std::time::Duration::from_millis(75);
        assert!(v.step_at(mid_t, 150).is_some());
        let mid = v.margin;
        assert!(mid > -400 && mid < 0, "mid={mid}");
        // 反向:从 mid 出发,不是从 -400。
        v.hide(150);
        let Vis::Animating {
            from,
            to,
            target,
            start,
        } = v.vis
        else {
            panic!("应重新进入动画")
        };
        assert_eq!(from, mid, "反向必须从当前实际位置出发");
        assert_eq!(to, -400);
        assert!(!target);
        // 走完:margin 回到 -400,状态 Hidden。
        let end_t = start + std::time::Duration::from_millis(200);
        v.step_at(end_t, 150);
        assert!(v.is_hidden());
        assert_eq!(v.margin, -400);
    }

    #[test]
    fn pin_toggles() {
        let mut v = VisibilityController::new(400);
        assert!(!v.pinned());
        assert!(v.toggle_pin());
        assert!(v.pinned());
    }

    #[test]
    fn layer_h_reanchor_when_hidden() {
        let mut v = VisibilityController::new(400);
        assert_eq!(v.margin, -400);
        v.set_layer_h(500);
        assert_eq!(v.margin, -500, "隐藏时重锚");
    }

    /// 回归:取整让 margin 提前到达终点后,靠时钟也必须收敛 ——
    /// "本次无需提交 margin" 不能阻止进入 Shown/Hidden。
    #[test]
    fn converges_on_time_even_when_margin_unchanged() {
        // 小高度:只有 -3..0 几个整数值,取整很快就停在 0。
        let mut v = VisibilityController::new(3);
        let t0 = Instant::now();
        v.show(150);
        // 90ms:ease-out 已到 ~0.97,margin 取整为 0;之后都"无变化"。
        assert!(v.step_at(t0 + Duration::from_millis(90), 150).is_some());
        // 140ms 与 160ms:margin 无变化,但 160ms 已到期,必须收敛。
        assert!(v.step_at(t0 + Duration::from_millis(140), 150).is_none());
        assert!(v.animating(), "140ms 未到期,仍在动画");
        assert!(v.step_at(t0 + Duration::from_millis(160), 150).is_none());
        assert!(v.is_shown(), "160ms 到期必须进入 Shown,即使 margin 未变");
        assert!(!v.animating());
        // 收敛后 step 是纯空操作。
        assert!(v.step_at(t0 + Duration::from_millis(200), 150).is_none());
    }

    /// 回归:隐藏方向同样收敛;且收敛后 margin 恰为目标值。
    #[test]
    fn hide_converges_with_consecutive_same_margin() {
        let mut v = VisibilityController::new(3);
        let t0 = Instant::now();
        v.show(0); // 直达 Shown。
        assert!(v.is_shown());
        v.hide(150);
        for ms in [30u64, 60, 90, 120, 140, 160] {
            v.step_at(t0 + Duration::from_millis(ms), 150);
        }
        assert!(v.is_hidden(), "隐藏动画到期必须收敛");
        assert_eq!(v.margin, -3);
    }

    /// 快速反向:动画中连续 show/hide/show,每次都从当前实际位置出发,
    /// 且最终按时收敛。
    #[test]
    fn rapid_reversal_stays_consistent() {
        let mut v = VisibilityController::new(400);
        let t0 = Instant::now();
        v.show(150);
        v.step_at(t0 + Duration::from_millis(50), 150);
        v.hide(150);
        v.step_at(t0 + Duration::from_millis(80), 150);
        v.show(150);
        v.step_at(t0 + Duration::from_millis(120), 150);
        assert!(v.animating(), "120ms 仍在(反向后的)动画");
        v.step_at(t0 + Duration::from_millis(400), 150);
        assert!(v.is_shown(), "反向后的呼出动画到期收敛");
        assert_eq!(v.margin, 0);
    }

    /// 零时长:从不进入 Animating,直接到位。
    #[test]
    fn zero_duration_never_animates() {
        let mut v = VisibilityController::new(400);
        assert!(matches!(v.show(0), VisOutcome::Margin(0)));
        assert!(!v.animating());
        assert!(matches!(v.hide(0), VisOutcome::Margin(-400)));
        assert!(!v.animating());
        // step 无动画可走。
        assert!(v.step(0).is_none());
    }

    /// 动画中尺寸变化:收起动画的目标同步更新,margin 不越界;
    /// 隐藏中变尺寸重锚;显示中变尺寸不影响 margin。
    #[test]
    fn layer_h_change_during_animation() {
        let mut v = VisibilityController::new(400);
        let t0 = Instant::now();
        v.show(0);
        v.hide(150);
        v.step_at(t0 + Duration::from_millis(30), 150);
        // 收起途中高度 400 → 300:目标从 -400 变为 -300。
        v.set_layer_h(300);
        let Vis::Animating { to, .. } = v.vis else {
            panic!("应在动画中")
        };
        assert_eq!(to, -300, "收起目标随高度更新");
        assert!(v.margin >= -300, "margin 不得比新高度更深");
        v.step_at(t0 + Duration::from_millis(500), 150);
        assert!(v.is_hidden());
        assert_eq!(v.margin, -300);
    }
}
