//! 会话状态:终端内核 + 锚点元数据(第八轮,架构 §3/§4)。
//!
//! 核心不变量:**Term 与 SessionMeta 合入同一把锁** —— 任何一次
//! 网格读取都配对产生它的累计滚动位移与版本号,不存在「新网格 +
//! 旧 cum」的中间态。坐标转换全部集中在本模块,UI 不做手工加减。
//!
//! 坐标模型:
//! - `HistoryAnchor`:grid 行 + cum_scroll,只增不减的稳定行号;
//! - `GridPoint`:网格行(可为负 = 滚动回看)与列;
//! - `DisplayPoint`:屏幕显示行与列;
//! - `SessionMeta::resolve`:锚点 → 网格行的唯一转换点,锚点早于
//!   历史淘汰时返回 None(显式失效,不猜位置)。

use std::sync::{Arc, mpsc};

use alacritty_terminal::event::EventListener;
use alacritty_terminal::event::WindowSize;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::Point;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vi_mode::ViMotion;
use alacritty_terminal::vte::ansi::{Color, NamedColor};

/// 历史绝对行(grid 行 + cum_scroll)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HistoryAnchor(pub i32);

/// 网格点(行可为负 = 滚动回看)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridPoint {
    pub line: i32,
    pub col: u32,
}

/// 屏幕显示点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayPoint {
    pub row: usize,
    pub col: usize,
}

/// 会话版本:每次语义变更(输出批次、resize、滚动、选区)单调 +1。
/// 复制/搜索等应答携带版本号,过期坐标上的操作应拒绝并重试。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct SessionRevision(pub u64);

/// 锚点失效世代:清屏/重置、resize/reflow、主/备屏幕切换时 +1。
/// 依赖历史锚点的请求(复制/跳转)必须携带**来源快照的世代** ——
/// 会话执行前校验,不匹配 = 坐标来源已失效,明确返回过期结果
/// (不猜测坐标)。**普通输出滚屏不推进世代**:稳定锚点继续有效,
/// 持续输出下复制不受影响(不以 revision 差异作粗略拒绝)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct AnchorEpoch(pub u64);

/// 会话元数据(与 Term 同锁)。
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionMeta {
    /// 累计滚动位移(只增不减;绝对行 - cum_scroll = 当前 grid 行)。
    /// 由 CountingHandler 按内核语义精确计数(见 ptyloop.rs)。
    pub cum_scroll: i32,
    /// 当前版本号(每次语义变更单调 +1)。
    pub revision: SessionRevision,
    /// 锚点失效世代(清屏/reset/resize/屏幕切换时 +1):UI 依据它
    /// 丢弃基于旧网格的视图状态(鼠标选区/QuickSelect/折叠);
    /// 锚点请求携带来源世代,会话执行前校验。
    pub invalidate_epoch: AnchorEpoch,
}

impl SessionMeta {
    /// 锚点 → 网格行(唯一转换点)。仅接受存活历史与屏幕内的行；
    /// 越界、无效尺寸或坐标运算溢出均显式失效。
    pub fn resolve(
        &self,
        anchor: HistoryAnchor,
        total_lines: usize,
        screen_lines: usize,
    ) -> Option<i32> {
        let grid = anchor.0.checked_sub(self.cum_scroll)?;
        let history = i32::try_from(total_lines.checked_sub(screen_lines)?).ok()?;
        let screen = i32::try_from(screen_lines).ok()?;
        (grid >= -history && grid < screen).then_some(grid)
    }
}

/// 快照单元(可见区一格;颜色在 UI 侧按配置解析)。
#[derive(Debug, Clone)]
pub struct SnapshotCell {
    pub c: char,
    pub zerowidth: Option<Arc<str>>,
    pub fg: Color,
    pub bg: Color,
    pub flags: Flags,
}

/// 会话快照(架构 §3):可见区域 + 锚点版本,由会话线程在每次
/// 语义变更后重建并发布(单槽覆盖,慢 UI 不积压)。**只复制可见
/// 行,不复制全部历史**;UI 渲染只读快照,不再直接接触 Term。
/// 快照携带产生它的 SessionMeta —— 网格与锚点必然同一版本。
/// (Colors 无 Debug,不派生。)
pub struct SessionSnapshot {
    pub meta: SessionMeta,
    /// (cols, rows)。
    pub dims: (usize, usize),
    pub total_lines: usize,
    pub display_offset: usize,
    pub cursor: GridPoint,
    /// 行主序可见区单元(rows × cols)。
    pub cells: Vec<SnapshotCell>,
    /// vi 模式选区(渲染高亮;鼠标选区在 UI 侧)。
    pub selection: Option<alacritty_terminal::selection::SelectionRange>,
    /// term 颜色表(OSC 可改,快照携带;UI 按配置解析)。
    pub colors: Colors,
    /// 终端模式位(输入路由/按键翻译用)。
    pub mode: TermMode,
    /// 光标形状(vi 模式为 Beam;渲染用)。
    pub cursor_shape: alacritty_terminal::vte::ansi::CursorShape,
    /// 命令块(COW 共享引用;协议语义状态,会话侧维护)。
    pub blocks: Arc<crate::blocks::CommandBlockStore>,
    /// 图像放置实例(COW 共享引用;解码像素经 Arc 共享)。
    pub images: Arc<Vec<KittyImageEntry>>,
}

/// 图像放置实例(锚定在网格行;解码像素经 KittyImage.rgba 共享)。
#[derive(Debug, Clone)]
pub struct KittyImageEntry {
    pub id: u32,
    pub grid_line: i32,
    pub col: u32,
    pub img: crate::kitty::KittyImage,
}

/// 语义事件(OSC 133 命令块 / 清屏 / kitty 图像):随原子批次
/// (SessionUpdate)发布 —— 与快照一起取出,绝不与网格脱节。
#[derive(Debug)]
pub enum BlockEvent {
    /// 提示符开始(历史绝对行 + 列;列用于「无末尾换行输出」的
    /// 复制边界 —— 提示符与输出同行时,列号即输出的结束位置)。
    PromptStart { line: i32, col: u32 },
    /// 命令输出开始(OSC 133;C,历史绝对行;块输出的干净起点)。
    OutputStart(i32),
    /// 上一条命令退出码。
    Exit { code: i32 },
    /// 清屏/重置(ED2/RIS):网格行号重排,锚点全部失效。
    Invalidated,
    /// kitty graphics 图像(放置于流中光标处;行号为历史绝对行)。
    Image {
        img: crate::kitty::KittyImage,
        line: i32,
        col: u32,
    },
}

/// 原子发布单元(架构 §7):语义状态(命令块/图像/锚点)**归会话
/// 线程所有**,快照以 COW 共享引用携带它们 —— UI 一次取出即得
/// 「同一版本的网格 + 语义状态」,不存在批次混合;慢 UI 下旧的
/// 显示快照被覆盖,语义状态在会话侧持续累积(不丢、不积压队列)。
pub struct SessionUpdate {
    pub snapshot: Arc<SessionSnapshot>,
    pub revision: SessionRevision,
}

/// UI → 会话命令(架构 §2.2):UI 不再直接修改 Term。
#[derive(Debug)]
pub enum SessionCommand {
    /// 键盘/粘贴/IME 文本。
    Input(Vec<u8>),
    ProtocolQuery(alacritty_terminal::event::Event),
    SetColors(tay_config::Colors),
    /// resize:PTY 与终端内核一并更新。
    Resize(WindowSize),
    /// 鼠标滚轮/触控板滚动(display_offset)。
    ScrollDisplay(Scroll),
    ScrollToOffset {
        offset: usize,
        epoch: AnchorEpoch,
    },
    /// 跳到历史绝对锚点(会话侧解析 + 世代校验 + 淘汰校验,
    /// 过期目标 no-op)。`epoch` = 锚点来源快照的失效世代。
    ScrollToAnchor {
        anchor: HistoryAnchor,
        epoch: AnchorEpoch,
    },
    /// vi 复制模式开关。
    ToggleVi,
    /// vi 动作(光标移动/词边界等)。
    ViMotion(ViMotion),
    /// vi 增量搜索(query, dir ±1)。
    Search {
        query: String,
        dir: i32,
        reply: mpsc::Sender<SessionReply>,
    },
    /// 复制:历史绝对锚点范围(唯一稳定坐标系 —— 滚屏后仍指向
    /// 同一文本;会话侧解析 + 裁剪到存活历史)。`to_col = None`
    /// 表示到行尾。`epoch` = 锚点来源快照的失效世代:会话执行前
    /// 校验,不匹配 → 明确返回过期(Stale),不猜测坐标。
    CopyAnchors {
        from_line: HistoryAnchor,
        from_col: u32,
        to_line: HistoryAnchor,
        to_col: Option<u32>,
        epoch: AnchorEpoch,
        reply: mpsc::Sender<SessionReply>,
    },
    /// vi 模式 yank(选区文本)。
    SelectionToString {
        reply: mpsc::Sender<SessionReply>,
    },
    Shutdown,
}

/// 会话应答(命令的回执;携带版本信息,过期结果由调用方按
/// revision 丢弃 —— 本轮复制/搜索应答即带快照版本)。
#[derive(Debug)]
pub enum SessionReply {
    /// 复制结果:`stale` = 世代校验失败(来源快照已失效 —— 清屏/
    /// resize/屏幕切换发生在请求来源之后),UI 应给出轻量反馈且
    /// **不覆盖已有剪贴板**;`stale = false` 且 `text = None` =
    /// 锚点已被历史淘汰(内容滚出滚动历史)。
    Copy {
        text: Option<String>,
        revision: SessionRevision,
        stale: bool,
    },
    Search {
        found: bool,
        revision: SessionRevision,
    },
    Selection {
        text: Option<String>,
        revision: SessionRevision,
    },
}

/// 世代校验后的复制结果(会话侧唯一执行入口的返回值)。
#[derive(Debug, PartialEq, Eq)]
pub enum CopyOutcome {
    Ok(String),
    /// 世代不匹配:锚点来源快照已失效(清屏/resize/屏幕切换)。
    Stale,
    /// 世代匹配,但锚点已被历史淘汰或范围无效。
    Evicted,
}

/// 会话状态:**Term + 语义状态(命令块/图像/锚点)的唯一所有者**
/// (架构 §3)。UI 经快照的 COW 共享引用读取。
pub struct SessionState<U: EventListener> {
    pub term: Term<U>,
    pub palette: tay_config::Colors,
    pub meta: SessionMeta,
    pub blocks: Arc<crate::blocks::CommandBlockStore>,
    pub images: Arc<Vec<KittyImageEntry>>,
}

impl<U: EventListener> SessionState<U> {
    pub fn new(term: Term<U>) -> Self {
        Self {
            term,
            palette: tay_config::Colors::default(),
            meta: SessionMeta::default(),
            blocks: Arc::new(crate::blocks::CommandBlockStore::default()),
            images: Arc::new(Vec::new()),
        }
    }

    /// 应用一条协议语义事件(PromptStart/OutputStart/Exit/
    /// Invalidated/Image)。COW:共享给快照的旧版本保持不变。
    pub fn apply_event(&mut self, ev: BlockEvent) {
        match ev {
            BlockEvent::PromptStart { line, col } => {
                let blocks = Arc::make_mut(&mut self.blocks);
                blocks.push_prompt(line, col);
                // 历史淘汰对齐:早于最老锚点的图像一并移除。
                if let Some(first) = blocks.first_prompt() {
                    Arc::make_mut(&mut self.images).retain(|e| e.grid_line >= first.0);
                }
            }
            BlockEvent::OutputStart(line) => {
                Arc::make_mut(&mut self.blocks).set_output_start(line);
            }
            BlockEvent::Exit { code } => {
                Arc::make_mut(&mut self.blocks).set_exit(code);
            }
            BlockEvent::Invalidated => {
                // 清屏/重置:锚点全部失效,图像一并清掉;网格失效
                // 世代 +1(UI 据此丢弃旧视图状态)。
                Arc::make_mut(&mut self.blocks).invalidate();
                self.images = Arc::new(Vec::new());
                self.meta.invalidate_epoch.0 += 1;
            }
            BlockEvent::Image { img, line, col } => {
                if img.action == crate::kitty::KittyAction::Delete {
                    if img.id == 0 {
                        // 协议:a=d 不带键 = 删除全部可见图像。
                        self.images = Arc::new(Vec::new());
                    } else {
                        Arc::make_mut(&mut self.images).retain(|e| e.id != img.id);
                    }
                    return;
                }
                let images = Arc::make_mut(&mut self.images);
                images.retain(|e| e.id != img.id);
                images.push(KittyImageEntry {
                    id: img.id,
                    grid_line: line,
                    col,
                    img,
                });
                if images.len() > 64 {
                    images.drain(..images.len() - 64);
                }
                // 解码像素总量预算(架构 §6):放置实例与资源一一对应
                // (同 id 替换,不共享),每张只计一次。
                const MAX_TOTAL_IMAGE_BYTES: usize = 64 * 1024 * 1024;
                let sizes: Vec<usize> = images.iter().map(|e| e.img.rgba.len()).collect();
                let drop = crate::kitty::eviction_count(&sizes, MAX_TOTAL_IMAGE_BYTES);
                if drop > 0 {
                    log::debug!("图像像素总量超预算,逐出 {drop} 张最旧图像");
                    images.drain(..drop);
                }
            }
        }
    }

    /// 语义变更后调用:版本 +1(输出批次、resize、滚动、选区)。
    pub fn bump(&mut self) {
        self.meta.revision.0 += 1;
    }

    /// 世代校验:锚点请求的来源世代与当前一致。
    /// 不一致 = 清屏/重置/resize/主备屏幕切换发生在来源快照之后,
    /// 坐标不可信 —— 调用方明确返回过期,不猜测坐标。
    /// 普通输出滚屏不推进世代,稳定锚点在持续输出下仍可复制。
    pub fn epoch_current(&self, epoch: AnchorEpoch) -> bool {
        self.meta.invalidate_epoch == epoch
    }

    /// 坐标身份变化但**内容锚点本身未失效**(主/备屏幕切换):
    /// 主屏网格在备用屏幕期间冻结,块/图像锚点保持有效 —— 只推进
    /// 世代,使跨屏幕的请求被拒绝;清空语义状态的场景走
    /// `apply_event(Invalidated)`。
    pub fn advance_epoch(&mut self) {
        self.meta.invalidate_epoch.0 += 1;
    }

    /// 重建可见区快照(不复制全部历史;每批输出/每次状态变更后
    /// 由会话线程调用并发布到单槽)。
    pub fn snapshot(&self) -> SessionSnapshot {
        let grid = self.term.grid();
        let cols = Dimensions::columns(grid);
        let rows = Dimensions::screen_lines(grid);
        let content = self.term.renderable_content();
        let mut cells = Vec::with_capacity(rows * cols);
        for idx in content.display_iter {
            if idx.point.column.0 >= cols {
                continue;
            }
            cells.push(SnapshotCell {
                c: idx.cell.c,
                zerowidth: idx
                    .cell
                    .zerowidth()
                    .filter(|c| !c.is_empty())
                    .map(|c| c.iter().collect::<String>().into()),
                fg: idx.cell.fg,
                bg: idx.cell.bg,
                flags: idx.cell.flags,
            });
        }
        cells.resize(
            rows * cols,
            SnapshotCell {
                c: ' ',
                zerowidth: None,
                fg: Color::Named(NamedColor::Foreground),
                bg: Color::Named(NamedColor::Background),
                flags: Flags::empty(),
            },
        );
        SessionSnapshot {
            meta: SessionMeta {
                cum_scroll: self.meta.cum_scroll,
                revision: self.meta.revision,
                invalidate_epoch: self.meta.invalidate_epoch,
            },
            dims: (cols, rows),
            total_lines: Dimensions::total_lines(grid),
            display_offset: grid.display_offset(),
            cursor: GridPoint {
                line: content.cursor.point.line.0,
                col: content.cursor.point.column.0 as u32,
            },
            cells,
            selection: content.selection,
            colors: *content.colors,
            mode: *self.term.mode(),
            cursor_shape: content.cursor.shape,
            blocks: self.blocks.clone(),
            images: self.images.clone(),
        }
    }

    /// 按块复制(绝对行范围):会话侧解析 + 裁剪到存活历史;
    /// 结果携带当前版本号。
    /// 复制(绝对锚点范围):会话侧解析 + 裁剪到存活历史。
    /// 锚点 = grid + cum,滚屏后仍指向同一文本(P1:异步鼠标复制
    /// 不再受请求前的滚屏影响);越界列钳制,已淘汰 → None。
    pub fn copy_anchors(
        &self,
        from_line: HistoryAnchor,
        from_col: u32,
        to_line: HistoryAnchor,
        to_col: Option<u32>,
    ) -> Option<String> {
        let grid = self.term.grid();
        let total = Dimensions::total_lines(grid);
        let screen = Dimensions::screen_lines(grid);
        let from_grid = self.meta.resolve(from_line, total, screen)?;
        let to_grid = self.meta.resolve(to_line, total, screen)?;
        if to_grid < from_grid {
            return None;
        }
        let cols = Dimensions::columns(grid).saturating_sub(1) as u32;
        Some(self.term.bounds_to_string(
            alacritty_terminal::index::Point::new(
                alacritty_terminal::index::Line(from_grid),
                alacritty_terminal::index::Column(from_col.min(cols) as usize),
            ),
            alacritty_terminal::index::Point::new(
                alacritty_terminal::index::Line(to_grid),
                alacritty_terminal::index::Column(
                    to_col.map(|c| c.min(cols)).unwrap_or(cols) as usize
                ),
            ),
        ))
    }

    /// 世代校验 + 锚点复制(会话侧唯一执行入口):`epoch` 与当前
    /// 世代不匹配 → `Stale`(来源快照已失效,不猜测坐标);
    /// 世代匹配但锚点被历史淘汰/范围无效 → `Evicted`;
    /// 成功 → `Ok(text)`。普通输出滚屏不推进世代 —— 持续输出下
    /// 旧世代请求照常复制原文本(不以 revision 差异粗略拒绝)。
    pub fn copy_anchors_checked(
        &self,
        from_line: HistoryAnchor,
        from_col: u32,
        to_line: HistoryAnchor,
        to_col: Option<u32>,
        epoch: AnchorEpoch,
    ) -> CopyOutcome {
        if !self.epoch_current(epoch) {
            return CopyOutcome::Stale;
        }
        match self.copy_anchors(from_line, from_col, to_line, to_col) {
            Some(text) => CopyOutcome::Ok(text),
            None => CopyOutcome::Evicted,
        }
    }

    /// resize(会话侧统一入口):PTY 尺寸由调用方同步;终端内核
    /// 重排。返回「网格尺寸是否实际变化」—— 仅变化时块/图像
    /// 锚点失效并推进失效代数(surface 重建后同尺寸 Resize 不得
    /// 清空语义状态,TAY_REBIND 验收复现的徽章丢失)。
    pub fn resize(&mut self, cols: usize, rows: usize) -> bool {
        let changed = Dimensions::columns(self.term.grid()) != cols
            || Dimensions::screen_lines(self.term.grid()) != rows;
        self.term.resize(TermDims { cols, rows });
        if changed {
            Arc::make_mut(&mut self.blocks).invalidate();
            self.images = Arc::new(Vec::new());
            self.meta.invalidate_epoch.0 += 1;
        }
        self.bump();
        changed
    }

    /// 跳到历史绝对锚点:会话侧解析;世代不匹配或已被淘汰则
    /// no-op(过期目标不猜位置)。
    pub fn scroll_to_anchor(&mut self, anchor: HistoryAnchor, epoch: AnchorEpoch) -> bool {
        if !self.epoch_current(epoch) {
            return false;
        }
        let grid = self.term.grid();
        let total = Dimensions::total_lines(grid);
        let screen = Dimensions::screen_lines(grid);
        let Some(line) = self.meta.resolve(anchor, total, screen) else {
            return false;
        };
        self.term.scroll_to_point(Point::new(
            alacritty_terminal::index::Line(line),
            alacritty_terminal::index::Column(0),
        ));
        true
    }
}

/// 终端内核 resize 用尺寸(仅会话线程构造)。
pub struct TermDims {
    pub cols: usize,
    pub rows: usize,
}

impl Dimensions for TermDims {
    fn columns(&self) -> usize {
        self.cols
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn total_lines(&self) -> usize {
        self.rows + 10_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_resolution_and_history_eviction() {
        let meta = SessionMeta {
            cum_scroll: 7,
            revision: SessionRevision(3),
            invalidate_epoch: AnchorEpoch::default(),
        };
        // 5 行屏幕 + 3 行历史 = total 8;grid 最老行 = -(8-5) = -3。
        // 锚点 4 = grid -3:仍在历史中。
        assert_eq!(meta.resolve(HistoryAnchor(4), 8, 5), Some(-3));
        // 锚点 3 = grid -4:已被淘汰 → 显式失效。
        assert_eq!(meta.resolve(HistoryAnchor(3), 8, 5), None);
        // 屏幕上:锚点 10 = grid 3。
        assert_eq!(meta.resolve(HistoryAnchor(10), 8, 5), Some(3));
    }

    #[test]
    fn revision_is_monotonic() {
        let mut s = SessionMeta::default();
        assert_eq!(s.revision, SessionRevision(0));
        s.revision.0 += 1;
        s.revision.0 += 1;
        assert_eq!(s.revision, SessionRevision(2));
    }

    #[test]
    fn anchor_resolution_rejects_future_rows_and_overflow() {
        let mut meta = SessionMeta::default();
        assert_eq!(meta.resolve(HistoryAnchor(4), 5, 5), Some(4));
        assert_eq!(meta.resolve(HistoryAnchor(5), 5, 5), None);
        assert_eq!(meta.resolve(HistoryAnchor(i32::MAX), 5, 5), None);
        assert_eq!(meta.resolve(HistoryAnchor(0), 0, 0), None);
        meta.cum_scroll = 1;
        assert_eq!(meta.resolve(HistoryAnchor(i32::MIN), 5, 5), None);
    }

    #[test]
    fn out_of_screen_anchor_requests_are_rejected() {
        let mut state = SessionState::new(epoch_test_term());
        let epoch = state.meta.invalidate_epoch;
        assert!(matches!(
            state.copy_anchors_checked(HistoryAnchor(0), 0, HistoryAnchor(5), None, epoch),
            CopyOutcome::Evicted
        ));
        assert!(!state.scroll_to_anchor(HistoryAnchor(5), epoch));
    }

    // —— 锚点世代(架构 §7:绑定失效世代的定点修复)——

    #[test]
    fn snapshots_preserve_combining_marks_and_nerd_font_characters() {
        let mut state = SessionState::new(epoch_test_term());
        let mut parser: alacritty_terminal::vte::ansi::Processor =
            alacritty_terminal::vte::ansi::Processor::new();
        parser.advance(
            &mut state.term,
            "e\u{301} a\u{308} \u{f120} \u{f02a2}".as_bytes(),
        );
        let snap = state.snapshot();
        assert_eq!(snap.cells[0].c, 'e');
        assert_eq!(snap.cells[0].zerowidth.as_deref(), Some("\u{301}"));
        assert_eq!(snap.cells[2].c, 'a');
        assert_eq!(snap.cells[2].zerowidth.as_deref(), Some("\u{308}"));
        assert_eq!(snap.cells[4].c, '\u{f120}');
        assert_eq!(snap.cells[6].c, '\u{f02a2}');
        assert_eq!(snap.cursor.col, 7);
    }

    /// 测试用终端:40 列 × 5 行,历史上限 20。
    fn epoch_test_term() -> Term<alacritty_terminal::event::VoidListener> {
        use alacritty_terminal::term::Config as TermConfig;

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
                self.rows + 20
            }
        }
        Term::new(
            TermConfig {
                scrolling_history: 20,
                ..Default::default()
            },
            &Dims { cols: 40, rows: 5 },
            alacritty_terminal::event::VoidListener,
        )
    }

    /// 喂一行并精确维护 cum_scroll(与 CountingHandler 的全屏
    /// linefeed 语义一致:换行发生在底行 → 滚动 +1;测试行短,
    /// 不触发 DECAWM 换行滚动,滚动区域为全屏默认值)。
    fn feed_line(
        parser: &mut alacritty_terminal::vte::ansi::Processor,
        state: &mut SessionState<alacritty_terminal::event::VoidListener>,
        cum: &mut i32,
        rows: usize,
        line: &str,
    ) {
        let at_bottom = state.term.renderable_content().cursor.point.line.0 as usize == rows - 1;
        parser.advance(&mut state.term, format!("{line}\r\n").as_bytes());
        if at_bottom {
            *cum += 1;
        }
        state.meta.cum_scroll = *cum;
    }

    /// 普通输出滚屏不推进世代:选择后继续输出,旧世代请求仍复制
    /// 原文本(不以「revision 不同」作粗略拒绝 —— 持续输出下
    /// 必须能正常复制)。
    #[test]
    fn scroll_keeps_epoch_and_copies_original_text() {
        let term = epoch_test_term();
        let mut state = SessionState::new(term);
        let mut parser: alacritty_terminal::vte::ansi::Processor =
            alacritty_terminal::vte::ansi::Processor::new();
        let mut cum = 0;
        feed_line(&mut parser, &mut state, &mut cum, 5, "select me");
        let epoch = state.meta.invalidate_epoch;
        let anchor = HistoryAnchor(0);
        // 继续输出 10 行 → 网格滚动,但世代不变。
        for i in 0..10 {
            feed_line(&mut parser, &mut state, &mut cum, 5, &format!("more {i}"));
        }
        assert_eq!(cum, 7, "5 行窗口输出 11 行 → 滚动 7 行");
        assert_eq!(state.meta.invalidate_epoch, epoch, "普通滚屏不推进世代");
        assert_eq!(
            state.copy_anchors_checked(anchor, 0, anchor, None, epoch),
            CopyOutcome::Ok("select me".to_string()),
            "旧世代请求在滚屏后复制原文本"
        );
    }

    /// 请求执行前清屏并写入新内容 → 旧选择被拒绝(Stale),
    /// 绝不把新文本当旧选择复制。
    #[test]
    fn stale_epoch_is_rejected_not_miscopied() {
        let term = epoch_test_term();
        let mut state = SessionState::new(term);
        let mut parser: alacritty_terminal::vte::ansi::Processor =
            alacritty_terminal::vte::ansi::Processor::new();
        let mut cum = 0;
        feed_line(&mut parser, &mut state, &mut cum, 5, "old text");
        let epoch = state.meta.invalidate_epoch;
        let anchor = HistoryAnchor(0);
        // 请求执行前:清屏(内核清屏 + 语义失效,与真实管线一致;
        // shell 的 clear = ED2 + 光标归位)+ 写入新内容。旧行号(0)
        // 仍在合法范围,却对应新文本。
        parser.advance(&mut state.term, b"\x1b[2J\x1b[H");
        state.apply_event(BlockEvent::Invalidated);
        feed_line(&mut parser, &mut state, &mut cum, 5, "new text");
        assert_ne!(state.meta.invalidate_epoch, epoch);
        assert_eq!(
            state.copy_anchors_checked(anchor, 0, anchor, None, epoch),
            CopyOutcome::Stale,
            "旧世代请求明确返回过期,不复制新文本"
        );
        // 新世代请求(UI 重新选择)正常复制新内容。
        let epoch_now = state.meta.invalidate_epoch;
        assert_eq!(
            state.copy_anchors_checked(anchor, 0, anchor, None, epoch_now),
            CopyOutcome::Ok("new text".to_string())
        );
    }

    /// resize/reflow 推进世代,旧选择被拒绝;同尺寸 resize 不推进
    /// (surface 重建后同尺寸 Resize 不得清空视图状态)。
    #[test]
    fn resize_advances_epoch_only_on_dim_change() {
        let term = epoch_test_term();
        let mut state = SessionState::new(term);
        let epoch = state.meta.invalidate_epoch;
        // 同尺寸(重建场景):世代不动。
        assert!(!state.resize(40, 5));
        assert_eq!(state.meta.invalidate_epoch, epoch);
        // 尺寸变化:世代 +1。
        assert!(state.resize(30, 6));
        assert_eq!(state.meta.invalidate_epoch.0, epoch.0 + 1);
    }

    /// 历史淘汰后返回明确失效(Evicted,与 Stale 区分 —— 前者是
    /// 内容滚出历史,后者是坐标来源已失效)。
    #[test]
    fn evicted_anchor_is_evicted_not_stale() {
        let term = epoch_test_term();
        let mut state = SessionState::new(term);
        let mut parser: alacritty_terminal::vte::ansi::Processor =
            alacritty_terminal::vte::ansi::Processor::new();
        let mut cum = 0;
        feed_line(&mut parser, &mut state, &mut cum, 5, "first line");
        let epoch = state.meta.invalidate_epoch;
        let first_anchor = HistoryAnchor(0);
        // 输出 30 行:5 行窗口 + 20 历史上限,最老行被淘汰。
        for i in 0..30 {
            feed_line(&mut parser, &mut state, &mut cum, 5, &format!("line {i}"));
        }
        assert_eq!(cum, 27, "31 行 → 27 次触底滚动");
        assert_eq!(
            state.copy_anchors_checked(first_anchor, 0, first_anchor, None, epoch),
            CopyOutcome::Evicted,
            "世代匹配但锚点已被历史淘汰 → 明确失效"
        );
        // 最新行仍可复制(同一世代)。
        let last = HistoryAnchor(30);
        assert_eq!(
            state.copy_anchors_checked(last, 0, last, None, epoch),
            CopyOutcome::Ok("line 29".to_string())
        );
    }

    /// 主/备屏幕坐标身份区分:切换推进世代,跨屏幕请求被拒绝;
    /// 主屏内容冻结,退出 alt 后新世代请求仍可复制主屏锚点;
    /// **块存储不受 alt 切换影响**(主屏块锚点保持有效 —— UI 侧
    /// 折叠按 BlockId 存活的前提,第十七轮回归:epoch 清理不得
    /// 清空折叠集合)。
    #[test]
    fn alt_screen_advances_epoch_and_separates_coordinates() {
        let term = epoch_test_term();
        let mut state = SessionState::new(term);
        let mut parser: alacritty_terminal::vte::ansi::Processor =
            alacritty_terminal::vte::ansi::Processor::new();
        let mut cum = 0;
        feed_line(&mut parser, &mut state, &mut cum, 5, "main screen");
        state.apply_event(BlockEvent::PromptStart { line: 0, col: 0 });
        let epoch_main = state.meta.invalidate_epoch;
        let main_anchor = HistoryAnchor(0);

        // 进 alt-screen(run 循环检测模式变化后调用 advance_epoch);
        // alt 网格的光标继承主屏位置(1049 保存/恢复语义),按实际
        // 光标行取锚点,不假设归位。
        parser.advance(&mut state.term, b"\x1b[?1049h");
        state.advance_epoch();
        assert_eq!(
            state.blocks.blocks().len(),
            1,
            "alt 切换推进世代但不清空块(主屏网格冻结,锚点仍有效)"
        );
        let alt_row = state.term.renderable_content().cursor.point.line.0;
        parser.advance(&mut state.term, b"vim buffer\r\n");
        assert_eq!(state.meta.invalidate_epoch.0, epoch_main.0 + 1);
        // 主屏世代的选择在 alt-screen 内被拒绝(坐标身份不同)。
        assert_eq!(
            state.copy_anchors_checked(main_anchor, 0, main_anchor, None, epoch_main),
            CopyOutcome::Stale
        );
        // alt 内新世代请求复制 alt 内容(相同 grid 行号,不同身份)。
        let epoch_alt = state.meta.invalidate_epoch;
        let alt_anchor = HistoryAnchor(alt_row + state.meta.cum_scroll);
        assert_eq!(
            state.copy_anchors_checked(alt_anchor, 0, alt_anchor, None, epoch_alt),
            CopyOutcome::Ok("vim buffer".to_string())
        );

        // 退出 alt:世代再 +1;alt 世代请求被拒;主屏锚点在新世代
        // 下仍有效(主屏网格冻结,alt 内滚动不计 cum —— 见 ptyloop
        // 的 counting_handler_ignores_alt_screen_scrolls)。
        parser.advance(&mut state.term, b"\x1b[?1049l");
        state.advance_epoch();
        assert_eq!(state.meta.invalidate_epoch.0, epoch_alt.0 + 1);
        assert_eq!(
            state.copy_anchors_checked(alt_anchor, 0, alt_anchor, None, epoch_alt),
            CopyOutcome::Stale,
            "alt 世代请求在主屏被拒"
        );
        let epoch_now = state.meta.invalidate_epoch;
        assert_eq!(
            state.copy_anchors_checked(main_anchor, 0, main_anchor, None, epoch_now),
            CopyOutcome::Ok("main screen".to_string()),
            "退出 alt 后主屏锚点在新世代下仍有效"
        );
    }
}
