//! 命令块存储(P3c:从 TayApp 拆出)。
//!
//! OSC 133 锚点(DESIGN §8.4):提示符行 A、输出起始行 C、退出码 D。
//! 折叠与徽章都按「块」工作,行号锚定网格行。
//!
//! 锚点有效性(2026-10-17 加固):
//! - **滚动不失效**:网格行号是绝对的,回看滚动只是视口移动;
//! - **历史淘汰失效**:超出滚动历史的行被网格回收 → 用 `max_span`
//!   (scrolling_history + 屏幕行数)做行距裁剪,保证锚点不悬空;
//! - **reflow/清屏失效**:resize 或 ED2/RIS 清屏后行号重排 → 调用方
//!   必须 `invalidate()`(清空全部锚点与折叠),下一条 prompt 重建;
//! - alt-screen 进出不失效:主屏幕内容冻结,行号不动。

use std::collections::BTreeSet;

use crate::session::HistoryAnchor;

/// 命令块数量上限(超出丢最旧,防止无界增长)。
const MAX_BLOCKS: usize = 200;

/// 块稳定 ID(存储内单调递增;折叠/跳转仍以 prompt 锚点寻址,
/// ID 供将来的增量更新与调试定位)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockId(pub u64);

/// 输出起始边界:区分协议提供的干净起点与缺少集成时的近似。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputBoundary {
    /// OSC 133;C 提供的干净输出起点(历史绝对行)。
    Protocol(HistoryAnchor),
    /// 无 shell 集成(无 C 标记):近似边界,退回 prompt+1。
    Approximate,
}

/// 命令块(第八轮:含义显式的结构体,替代 `(i32,i32,Option,Option)` 元组)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandBlock {
    pub id: BlockId,
    /// 提示符锚点(历史绝对行)。
    pub prompt: HistoryAnchor,
    /// 提示符列:无末尾换行输出的复制边界(提示符与输出同行时,
    /// 结束列 = prompt 列 - 1)。
    pub prompt_col: u32,
    /// 输出起始边界(C 协议边界 / 近似)。
    pub output_start: OutputBoundary,
    /// 退出码(OSC 133;D)。
    pub exit: Option<i32>,
}

/// 命令块存储:最新块在尾。**协议语义状态,归会话线程所有**
/// (COW Arc 共享给 UI 快照,架构 §3);折叠状态是 UI 视图状态,
/// 由 UI 按稳定 `BlockId` 持有,不随行号变化。
#[derive(Clone)]
pub struct CommandBlockStore {
    blocks: Vec<CommandBlock>,
    next_id: u64,
    /// 锚点行距上限 = 终端滚动历史 + 屏幕行数;首尾 prompt 行号差
    /// 超过它,最老锚点必然已被网格回收(历史淘汰)。
    max_span: i32,
}

impl Default for CommandBlockStore {
    /// 默认行距上限 = 10000 滚动历史 + 80 行屏幕余量。
    fn default() -> Self {
        Self::new(10_080)
    }
}

impl CommandBlockStore {
    /// `max_span` 取 `TermConfig::scrolling_history + 屏幕行数`
    /// (给 reflow 前最后一屏留余量)。
    pub fn new(max_span: i32) -> Self {
        Self {
            blocks: Vec::new(),
            next_id: 0,
            max_span: max_span.max(1),
        }
    }

    /// 提示符开始(OSC 133;A);`line` 为历史绝对行。
    pub fn push_prompt(&mut self, line: i32, col: u32) {
        let id = BlockId(self.next_id);
        self.next_id += 1;
        self.blocks.push(CommandBlock {
            id,
            prompt: HistoryAnchor(line),
            prompt_col: col,
            output_start: OutputBoundary::Approximate,
            exit: None,
        });
        self.trim(HistoryAnchor(line));
    }

    /// 按行距裁剪最老锚点(历史淘汰对齐)+ 数量上限。折叠状态在
    /// UI 侧(按 BlockId):被裁掉的块的 ID 随下一快照不再可见,
    /// UI 在应用快照时同步清理。
    fn trim(&mut self, latest: HistoryAnchor) {
        if let Some(first) = self.blocks.first().map(|b| b.prompt)
            && latest.0 - first.0 > self.max_span
        {
            let keep_from = HistoryAnchor(latest.0 - self.max_span);
            let cut = self.blocks.partition_point(|b| b.prompt < keep_from);
            self.blocks.drain(..cut);
        }
        if self.blocks.len() > MAX_BLOCKS {
            self.blocks.drain(..self.blocks.len() - MAX_BLOCKS);
        }
    }

    /// 锚点全部失效(reflow / 清屏 / 终端重置):清空块。
    /// 下一条 prompt(A)会重建。
    pub fn invalidate(&mut self) {
        self.blocks.clear();
    }

    /// 输出起始(OSC 133;C):当前块的干净输出起点。
    pub fn set_output_start(&mut self, line: i32) {
        if let Some(last) = self.blocks.last_mut()
            && last.output_start == OutputBoundary::Approximate
        {
            last.output_start = OutputBoundary::Protocol(HistoryAnchor(line));
        }
    }

    /// 退出码(OSC 133;D)。
    pub fn set_exit(&mut self, code: i32) {
        if let Some(last) = self.blocks.last_mut() {
            last.exit = Some(code);
        }
    }

    /// 折叠区间:相邻提示符之间、且被折叠(按稳定 BlockId 标记,
    /// UI 视图状态)的输出行范围(含端点,历史绝对行)。
    pub fn fold_ranges(&self, folded: &BTreeSet<BlockId>) -> Vec<(HistoryAnchor, HistoryAnchor)> {
        let mut ranges = Vec::new();
        for w in self.blocks.windows(2) {
            if folded.contains(&w[0].id) {
                let a = w[0].prompt.0 + 1;
                let b = w[1].prompt.0 - 1;
                if a <= b {
                    ranges.push((HistoryAnchor(a), HistoryAnchor(b)));
                }
            }
        }
        ranges
    }

    /// 全部块(只读;折叠/导航等 UI 视图逻辑用)。
    pub fn blocks(&self) -> &[CommandBlock] {
        &self.blocks
    }

    /// 某历史绝对行的非零退出码(徽章用)。
    pub fn nonzero_exit_at(&self, line: HistoryAnchor) -> Option<i32> {
        self.blocks
            .iter()
            .find(|b| b.prompt == line && b.exit.is_some_and(|c| c != 0))
            .and_then(|b| b.exit)
    }

    /// 包含历史绝对行 `line` 的块的提示符锚点(折叠切换用)。
    pub fn prompt_containing(&self, line: HistoryAnchor) -> Option<HistoryAnchor> {
        self.blocks
            .windows(2)
            .find(|w| line > w[0].prompt && line < w[1].prompt)
            .map(|w| w[0].prompt)
    }

    /// 非零退出码块(提示符锚点, 退出码),徽章构建用。
    pub fn badges(&self) -> impl Iterator<Item = (&HistoryAnchor, i32)> {
        self.blocks
            .iter()
            .filter_map(|b| b.exit.filter(|c| *c != 0).map(|c| (&b.prompt, c)))
    }

    /// 「上一条命令」的纯输出复制范围,`(from, end_line, end_col)`:
    /// `from` = C 行(无 C 退回 prompt+1);结束位置由下一提示符的
    /// (行, 列)决定 —— `end_col = None` 表示到行尾,`Some(c)` 表示
    /// 该行第 c 列(含)为止。
    ///
    /// 语义(2026-10-17 明确,第二轮修正):
    /// - **单行输出**:from == end_line,复制该行 —— 合法;
    /// - **空输出**(C 与下一提示符之间没有任何输出行):范围为空,
    ///   返回 None;"无输出的命令"与"恰好输出到 C 行的单行命令"
    ///   在行号上不可区分,单行命令会复制 C 行 —— 空输出场景该行
    ///   为空白,复制出空行,属可接受近似;
    /// - **无末尾换行**:输出与提示符同行 —— 提示符的 (行, 列) 即
    ///   输出结束位置,`end_col = 提示符列 - 1`,完整复制输出且
    ///   不含提示符(不再作为限制);
    /// - **多行提示符**:A 在 PS1 渲染前发出,C 在命令回显后 ——
    ///   提示符自身的行数不影响 from(C 已在其后)。
    pub fn last_output_range(&self) -> Option<(HistoryAnchor, HistoryAnchor, Option<u32>)> {
        let n = self.blocks.len();
        if n < 2 {
            return None;
        }
        let prev = &self.blocks[n - 2];
        let next = &self.blocks[n - 1];
        let from = match prev.output_start {
            OutputBoundary::Protocol(a) => a,
            OutputBoundary::Approximate => HistoryAnchor(prev.prompt.0 + 1),
        };
        if next.prompt_col == 0 {
            // 提示符在新行行首:输出结束于上一行行尾。
            let end_line = next.prompt.0 - 1;
            if from.0 > end_line {
                return None;
            }
            Some((from, HistoryAnchor(end_line), None))
        } else {
            // 提示符与输出同行(无末尾换行):结束于提示符前一列。
            let end_col = next.prompt_col - 1;
            if from.0 > next.prompt.0 {
                return None;
            }
            Some((from, next.prompt, Some(end_col)))
        }
    }

    /// 提示符跳转:top 历史绝对行之前/之后的最近提示符锚点。
    pub fn prompt_before(&self, top: HistoryAnchor) -> Option<HistoryAnchor> {
        self.blocks
            .iter()
            .rev()
            .map(|b| b.prompt)
            .find(|l| *l < top)
    }

    pub fn prompt_after(&self, top: HistoryAnchor) -> Option<HistoryAnchor> {
        self.blocks.iter().map(|b| b.prompt).find(|l| *l > top)
    }

    pub fn first_prompt(&self) -> Option<HistoryAnchor> {
        self.blocks.first().map(|b| b.prompt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_range_uses_c_boundary() {
        let mut b = CommandBlockStore::default();
        b.push_prompt(0, 0);
        b.set_output_start(1);
        b.set_exit(0);
        b.push_prompt(5, 0);
        let (from, to, end) = b.last_output_range().expect("两块应有范围");
        assert_eq!(
            (from, to, end),
            (HistoryAnchor(1), HistoryAnchor(4), None),
            "从 C 边界到下一提示符前一行行尾"
        );
    }

    #[test]
    fn output_range_falls_back_without_c() {
        let mut b = CommandBlockStore::default();
        b.push_prompt(0, 0);
        b.set_exit(0);
        b.push_prompt(5, 0);
        let (from, to, end) = b.last_output_range().unwrap();
        assert_eq!(
            (from, to, end),
            (HistoryAnchor(1), HistoryAnchor(4), None),
            "无 C 时退回 prompt+1"
        );
    }

    #[test]
    fn fold_ranges_and_toggle() {
        let mut b = CommandBlockStore::default();
        b.push_prompt(0, 0);
        b.push_prompt(3, 0);
        b.push_prompt(6, 0);
        // 折叠是 UI 视图状态:按稳定 BlockId 标记(不随行号变化)。
        let id = b.blocks()[1].id;
        let mut folded: BTreeSet<BlockId> = BTreeSet::new();
        folded.insert(id);
        assert_eq!(
            b.fold_ranges(&folded),
            vec![(HistoryAnchor(4), HistoryAnchor(5))]
        );
        folded.remove(&id);
        assert!(b.fold_ranges(&folded).is_empty());
    }

    #[test]
    fn trim_keeps_last_200_blocks() {
        let mut b = CommandBlockStore::default();
        for i in 0..210 {
            b.push_prompt(i, 0);
        }
        assert!(b.blocks().len() <= 200);
        assert!(b.first_prompt().unwrap().0 >= 10);
    }

    #[test]
    fn badges_and_navigation() {
        let mut b = CommandBlockStore::default();
        b.push_prompt(0, 0);
        b.set_exit(0);
        b.push_prompt(4, 0);
        b.set_exit(7);
        b.push_prompt(9, 0);
        assert_eq!(b.nonzero_exit_at(HistoryAnchor(4)), Some(7));
        assert_eq!(b.nonzero_exit_at(HistoryAnchor(0)), None);
        assert_eq!(b.prompt_before(HistoryAnchor(8)), Some(HistoryAnchor(4)));
        assert_eq!(b.prompt_after(HistoryAnchor(4)), Some(HistoryAnchor(9)));
    }

    /// P2 加固回归:单行输出必须可复制(from == to 含两端)。
    #[test]
    fn single_line_output_is_copyable() {
        let mut b = CommandBlockStore::default();
        b.push_prompt(0, 0);
        b.set_output_start(10);
        b.set_exit(0);
        b.push_prompt(11, 0);
        assert_eq!(
            b.last_output_range(),
            Some((HistoryAnchor(10), HistoryAnchor(10), None)),
            "单行输出是合法范围"
        );
    }

    /// P2 加固回归:空输出(C 与下一提示符之间无行)返回 None。
    #[test]
    fn empty_output_returns_none() {
        let mut b = CommandBlockStore::default();
        b.push_prompt(0, 0);
        b.set_output_start(10);
        b.set_exit(0);
        b.push_prompt(10, 0); // 提示符与 C 同行(无换行输出/空输出)。
        assert_eq!(b.last_output_range(), None);
    }

    /// P2 加固回归:行距超过滚动历史时,最老锚点被裁剪(历史淘汰对齐)。
    #[test]
    fn history_trim_drops_stale_anchors() {
        let mut b = CommandBlockStore::new(50);
        for i in 0..10 {
            b.push_prompt(i * 20, 0);
        }
        // 首尾行距 180 > 50:最老锚点全部失效。
        assert!(b.first_prompt().unwrap().0 >= 180 - 50);
        let _ = b.badges().count(); // 裁剪后仍可迭代徽章。
    }

    /// P2 加固回归:invalidate 清空块与折叠。
    #[test]
    fn invalidate_clears_everything() {
        let mut b = CommandBlockStore::default();
        b.push_prompt(0, 0);
        b.push_prompt(5, 0);
        b.invalidate();
        assert!(b.first_prompt().is_none());
        assert!(b.fold_ranges(&BTreeSet::new()).is_empty());
        assert!(b.last_output_range().is_none());
    }

    /// 真实终端网格驱动(P2 加固要求):用 alacritty Term 喂真实字节
    /// 流,以**真实光标行号**填充块存储,再用 bounds_to_string 验证
    /// 复制的正是命令的纯输出 —— 不依赖手工构造的行号。
    #[test]
    fn real_terminal_grid_drives_output_copy() {
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::term::{Config as TermConfig, Term};
        use alacritty_terminal::vte::ansi;

        struct Dims {
            cols: usize,
            rows: usize,
        }
        impl alacritty_terminal::grid::Dimensions for Dims {
            fn columns(&self) -> usize {
                self.cols
            }
            fn screen_lines(&self) -> usize {
                self.rows
            }
            fn total_lines(&self) -> usize {
                self.rows + 200
            }
        }

        let mut term = Term::new(
            TermConfig {
                scrolling_history: 200,
                ..Default::default()
            },
            &Dims { cols: 80, rows: 24 },
            VoidListener,
        );
        let mut parser: ansi::Processor = ansi::Processor::new();
        let mut store = CommandBlockStore::new(200 + 24 + 10);
        let mut feed = |bytes: &[u8], term: &mut Term<VoidListener>| parser.advance(term, bytes);
        let cursor_line = |term: &Term<VoidListener>| term.renderable_content().cursor.point.line.0;

        // 第一条命令:两行输出。
        let p0 = cursor_line(&term);
        store.push_prompt(p0, 0);
        feed(b"> cmd1\r\n", &mut term);
        let c1 = cursor_line(&term);
        store.set_output_start(c1);
        feed(b"out1\r\nout2\r\n", &mut term);
        store.set_exit(0);
        let p1 = cursor_line(&term);
        store.push_prompt(p1, 0);
        // 复制范围 = [C 行, 下一提示符前一行],内容恰是纯输出。
        let (from, to, end) = store.last_output_range().expect("两行输出应有范围");
        assert_eq!(
            (from, to, end),
            (HistoryAnchor(c1), HistoryAnchor(p1 - 1), None)
        );
        let text = term.bounds_to_string(
            alacritty_terminal::index::Point::new(
                alacritty_terminal::index::Line(from.0),
                alacritty_terminal::index::Column(0),
            ),
            alacritty_terminal::index::Point::new(
                alacritty_terminal::index::Line(to.0),
                alacritty_terminal::index::Column(79),
            ),
        );
        assert_eq!(
            text, "out1\nout2",
            "纯输出,末行无行终止符(bounds_to_string 语义)"
        );

        // 第二条命令:单行输出(带末尾换行)。
        feed(b"> cmd2\r\n", &mut term);
        let c2 = cursor_line(&term);
        store.set_output_start(c2);
        feed(b"single\r\n", &mut term);
        store.set_exit(0);
        let p2 = cursor_line(&term);
        store.push_prompt(p2, 0);
        let (from, to, end) = store.last_output_range().expect("单行输出应有范围");
        assert_eq!(
            (from, to, end),
            (HistoryAnchor(c2), HistoryAnchor(p2 - 1), None)
        );
        let text = term.bounds_to_string(
            alacritty_terminal::index::Point::new(
                alacritty_terminal::index::Line(from.0),
                alacritty_terminal::index::Column(0),
            ),
            alacritty_terminal::index::Point::new(
                alacritty_terminal::index::Line(to.0),
                alacritty_terminal::index::Column(79),
            ),
        );
        assert_eq!(text, "single", "单行输出完整复制(末行无行终止符)");

        // 第三条命令:空输出 → None。
        feed(b"> cmd3\r\n", &mut term);
        let c3 = cursor_line(&term);
        store.set_output_start(c3);
        store.set_exit(0);
        let p3 = cursor_line(&term);
        store.push_prompt(p3, 0);
        assert_eq!(store.last_output_range(), None, "空输出不可复制");

        // 多行提示符:提示符自身占两行,不影响 C 边界。
        feed(b"\r\n> \r\n", &mut term);
        let p4 = cursor_line(&term);
        store.push_prompt(p4, 0);
        feed(b"cmd4\r\n", &mut term);
        let c4 = cursor_line(&term);
        store.set_output_start(c4);
        feed(b"deep\r\n", &mut term);
        store.set_exit(0);
        let p5 = cursor_line(&term);
        store.push_prompt(p5, 0);
        let (from, to, end) = store.last_output_range().expect("多行提示符后输出可复制");
        assert_eq!(
            (from, to, end),
            (HistoryAnchor(c4), HistoryAnchor(p5 - 1), None)
        );
        let text = term.bounds_to_string(
            alacritty_terminal::index::Point::new(
                alacritty_terminal::index::Line(from.0),
                alacritty_terminal::index::Column(0),
            ),
            alacritty_terminal::index::Point::new(
                alacritty_terminal::index::Line(to.0),
                alacritty_terminal::index::Column(79),
            ),
        );
        assert_eq!(text, "deep");
    }

    /// P1 回归(用户报告):5 行窗口输出 10 行后,锚点必须随滚动
    /// 保持有效。锚点存**绝对行**(捕获时 grid 行 + history),
    /// 使用前减 history_now 转回 grid 行 —— 输出起点应落在 -6。
    #[test]
    fn real_terminal_scroll_keeps_anchors_valid() {
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::grid::Dimensions;
        use alacritty_terminal::term::{Config as TermConfig, Term};
        use alacritty_terminal::vte::ansi;

        struct Dims {
            cols: usize,
            rows: usize,
        }
        impl Dimensions for Dims {
            fn columns(&self) -> usize {
                self.cols
            }
            fn screen_lines(&self) -> usize {
                self.rows
            }
            fn total_lines(&self) -> usize {
                self.rows + 100
            }
        }
        let mut term = Term::new(
            TermConfig {
                scrolling_history: 100,
                ..Default::default()
            },
            &Dims { cols: 40, rows: 5 },
            VoidListener,
        );
        let mut parser: ansi::Processor = ansi::Processor::new();
        let mut store = CommandBlockStore::default();
        let hist =
            |t: &Term<VoidListener>| Dimensions::total_lines(t.grid()).saturating_sub(5) as i32;
        let cursor = |t: &Term<VoidListener>| t.renderable_content().cursor.point;

        // 提示符 → 命令回显 → C → 10 行输出 → D → 下一提示符。
        let p0 = cursor(&term);
        store.push_prompt(p0.line.0 + hist(&term), p0.column.0 as u32);
        parser.advance(&mut term, b"> cmd\r\n");
        let c = cursor(&term);
        store.set_output_start(c.line.0 + hist(&term));
        for i in 1..=10 {
            parser.advance(&mut term, format!("line{i}\r\n").as_bytes());
        }
        store.set_exit(0);
        let p1 = cursor(&term);
        store.push_prompt(p1.line.0 + hist(&term), p1.column.0 as u32);

        // 绝对范围:from = C 捕获绝对行,to = 下一提示符前一行。
        let (from, to, end) = store.last_output_range().expect("10 行输出可复制");
        assert_eq!(end, None);
        // 使用前转回 grid 行:此时 history = 7(10 行输出在 5 行窗口)。
        assert_eq!(hist(&term), 7, "滚动进历史的行数");
        let from_grid = from.0 - hist(&term);
        let to_grid = to.0 - hist(&term);
        assert_eq!(from_grid, -6, "输出起点落在 scrollback(用户报告值)");
        let text = term.bounds_to_string(
            alacritty_terminal::index::Point::new(
                alacritty_terminal::index::Line(from_grid),
                alacritty_terminal::index::Column(0),
            ),
            alacritty_terminal::index::Point::new(
                alacritty_terminal::index::Line(to_grid),
                alacritty_terminal::index::Column(39),
            ),
        );
        let expect: String = (1..=10)
            .map(|i| format!("line{i}\n"))
            .collect::<String>()
            .trim_end_matches('\n')
            .to_string();
        assert_eq!(text, expect, "复制内容完整(末行无终止符)");

        // 跳转:scroll_to_point 到输出起点 grid 行。
        term.scroll_to_point(alacritty_terminal::index::Point::new(
            alacritty_terminal::index::Line(from_grid),
            alacritty_terminal::index::Column(0),
        ));
        assert_eq!(term.grid().display_offset(), 6, "视口滚到输出起点");
    }

    /// 无末尾换行:输出与提示符同行 —— 结束于提示符前一列,
    /// 完整复制输出且不含提示符(第二轮加固)。
    #[test]
    fn real_terminal_no_trailing_newline_copies_output_only() {
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::term::{Config as TermConfig, Term};
        use alacritty_terminal::vte::ansi;

        struct Dims {
            cols: usize,
            rows: usize,
        }
        impl alacritty_terminal::grid::Dimensions for Dims {
            fn columns(&self) -> usize {
                self.cols
            }
            fn screen_lines(&self) -> usize {
                self.rows
            }
            fn total_lines(&self) -> usize {
                self.rows + 100
            }
        }
        let mut term = Term::new(
            TermConfig {
                scrolling_history: 100,
                ..Default::default()
            },
            &Dims { cols: 80, rows: 24 },
            VoidListener,
        );
        let mut parser: ansi::Processor = ansi::Processor::new();
        let mut store = CommandBlockStore::default();
        let cursor = |term: &Term<VoidListener>| term.renderable_content().cursor.point;

        let p0 = cursor(&term);
        store.push_prompt(p0.line.0, p0.column.0 as u32);
        // 命令回显 + 换行;输出 "no-newline"(无换行);提示符直接接在同一行。
        parser.advance(&mut term, b"> cmd\r\n");
        let c = cursor(&term);
        store.set_output_start(c.line.0);
        parser.advance(&mut term, b"no-newline");
        store.set_exit(0);
        let p1 = cursor(&term); // 提示符 A 在 "no-newline" 之后同一行。
        store.push_prompt(p1.line.0, p1.column.0 as u32);

        let (from, end_line, end_col) = store.last_output_range().expect("无末尾换行可复制");
        assert_eq!(from.0, c.line.0);
        assert_eq!(end_line.0, p1.line.0, "结束行 = 提示符所在行");
        let col = end_col.expect("结束列 = 提示符列 - 1");
        assert_eq!(col, p1.column.0 as u32 - 1);
        let text = term.bounds_to_string(
            alacritty_terminal::index::Point::new(
                alacritty_terminal::index::Line(from.0),
                alacritty_terminal::index::Column(0),
            ),
            alacritty_terminal::index::Point::new(
                alacritty_terminal::index::Line(end_line.0),
                alacritty_terminal::index::Column(col as usize),
            ),
        );
        assert_eq!(text, "no-newline", "输出完整且不含提示符");
    }
}
