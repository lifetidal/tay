//! 呼出重建路径的分段计时:StageTimer 记录各阶段毫秒并输出摘要日志。

use std::time::Instant;

/// 首帧计时锚点:内容帧计时优先用 content_at(缓存 blit 时刻或
/// 沿用 show_at 的旧口径);两者都没有时回退 show_at(首次启动
/// 握手路径不设 content_at,但 show_at 仍有效)。
pub fn first_frame_anchor(
    content_at: &mut Option<Instant>,
    show_at: &mut Option<Instant>,
) -> Option<Instant> {
    content_at.take().or_else(|| show_at.take())
}

/// 呼出重建路径的分段计时器。创建后各阶段用 `mark(name)` 记录,
/// `summary()` 输出「呼出阶段 a=12ms, b=8ms, 总 20ms」样式的日志行。
pub struct StageTimer {
    t0: Instant,
    pub marks: Vec<(&'static str, u128)>,
}

impl StageTimer {
    pub fn new() -> Self {
        Self {
            t0: Instant::now(),
            marks: Vec::new(),
        }
    }

    pub fn mark(&mut self, name: &'static str) {
        self.marks.push((name, self.t0.elapsed().as_millis()));
    }

    pub fn summary(&self) -> String {
        let parts = self
            .marks
            .iter()
            .map(|(n, ms)| format!("{n}={ms}ms"))
            .collect::<Vec<_>>()
            .join(", ");
        let total = self.marks.last().map(|(_, ms)| *ms).unwrap_or(0);
        if parts.is_empty() {
            format!("呼出阶段 总 {total}ms")
        } else {
            format!("呼出阶段 {parts}, 总 {total}ms")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_are_monotonic() {
        let mut t = StageTimer::new();
        t.mark("a");
        t.mark("b");
        t.mark("c");
        assert_eq!(t.marks.len(), 3);
        for w in t.marks.windows(2) {
            assert!(w[1].1 >= w[0].1, "mark 耗时必须非降: {:?}", t.marks);
        }
    }

    #[test]
    fn summary_contains_segments() {
        let mut t = StageTimer::new();
        t.mark("rebind");
        t.mark("configure");
        let s = t.summary();
        assert!(s.starts_with("呼出阶段"), "摘要前缀错误: {s}");
        assert!(s.contains("rebind"), "摘要缺段名 rebind: {s}");
        assert!(s.contains("configure"), "摘要缺段名 configure: {s}");
        assert!(s.contains("ms"), "摘要缺毫秒单位: {s}");
    }

    #[test]
    fn empty_summary_reports_zero_total() {
        let s = StageTimer::new().summary();
        assert!(s.contains("总 0ms"), "空计时器应报总 0ms: {s}");
    }

    #[test]
    fn anchor_prefers_content_at() {
        let now = Instant::now();
        let mut content = Some(now);
        let mut show = Some(now + std::time::Duration::from_secs(1));
        assert_eq!(first_frame_anchor(&mut content, &mut show), Some(now));
        assert!(show.is_some(), "未使用 show_at 不得消费");
    }

    #[test]
    fn anchor_falls_back_to_show_at() {
        let now = Instant::now();
        let mut content = None;
        let mut show = Some(now);
        assert_eq!(first_frame_anchor(&mut content, &mut show), Some(now));
        assert!(show.is_none(), "回退时必须消费 show_at");
    }

    #[test]
    fn anchor_both_none_is_none() {
        assert_eq!(first_frame_anchor(&mut None, &mut None), None);
    }
}
