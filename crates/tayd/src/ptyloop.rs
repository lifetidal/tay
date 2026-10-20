//! 自研 PTY 事件循环:替换 alacritty_terminal::event_loop::EventLoop。
//!
//! 为什么自研:alacritty 的事件循环把 PTY 读到的字节直接喂给 vte parser,
//! 前端看不到输出流 —— 而 OSC 133 命令块(DESIGN §8.4)需要前端消费。
//! 自研后读取路径归我们,顺手解析 OSC 133,其余行为与原版一致
//! (poll PTY + 唤醒管道、DEC 2026 同步超时、子进程退出通知)。

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::session::{BlockEvent, SessionCommand, SessionReply, SessionState, SessionUpdate};
use alacritty_terminal::event::{Event, EventListener, OnResize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::tty::{self, ChildEvent, EventedPty};
use alacritty_terminal::vte::ansi::{
    self, Attr, CharsetIndex, ClearMode, CursorShape, CursorStyle, Handler, Hyperlink,
    KeyboardModes, KeyboardModesApplyBehavior, LineClearMode, Mode, PrivateMode, Rgb,
    StandardCharset, TabulationClearMode,
};
use polling::{Event as PollEvent, Poller};

/// UI → 会话在途输入字节上限(**统一预算**,架构 §6):一个共享
/// 计数器同时覆盖「跨线程消息」与「PTY 待写队列」—— 预留直到
/// 字节实际写入 PTY、明确拒绝或会话结束才释放;已接受的输入不因
/// 内部第二次预算检查丢失(会话端不再单独设更小的队列预算)。
/// 超限**显式拒绝整块**(不静默截断、不破坏粘贴边界);
/// 控制命令(Resize/Shutdown 等)不受预算影响。
pub const MAX_INFLIGHT_INPUT_BYTES: usize = 512 * 1024;

/// 发送端预算判定(纯函数):在途 + 本次 ≤ 上限。
pub fn input_budget_ok(inflight: usize, incoming: usize) -> bool {
    inflight + incoming <= MAX_INFLIGHT_INPUT_BYTES
}

/// 协议应答(终端 → PTY)的额外余量:应答必须送达,不计入用户
/// 输入上限但计入在途 —— 在途总量超过「用户上限 + 余量」说明
/// 应用在输入积压期间高频查询终端(查询风暴),**告警**(不拒绝,
/// 拒绝会卡住应用)。正常交互远达不到。
pub const MAX_RESPONSE_SLACK_BYTES: usize = 256 * 1024;

/// 在途总量(用户输入 + 协议应答)的健康上限(纯函数)。
pub fn response_budget_ok(total_inflight: usize) -> bool {
    total_inflight <= MAX_INFLIGHT_INPUT_BYTES + MAX_RESPONSE_SLACK_BYTES
}

/// 原子预留:检查 + 预留一步完成(`fetch_update` 保证多发送者
/// 并发时总预留不突破上限 —— 检查与预留之间无窗口)。
fn reserve_input(inflight: &std::sync::atomic::AtomicUsize, len: usize) -> bool {
    inflight
        .fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |cur| input_budget_ok(cur, len).then_some(cur + len),
        )
        .is_ok()
}

/// 释放预留额度:饱和减 —— 会话退出统一清零后,并发的发送端
/// 释放不会下溢。
fn release_input(inflight: &std::sync::atomic::AtomicUsize, len: usize) {
    let _ = inflight.fetch_update(
        std::sync::atomic::Ordering::Relaxed,
        std::sync::atomic::Ordering::Relaxed,
        |cur| Some(cur.saturating_sub(len)),
    );
}

/// 消息的输入字节数(预算仅约束输入,控制命令不受影响)。
fn input_len(msg: &SessionCommand) -> Option<usize> {
    match msg {
        SessionCommand::Input(bytes) => Some(bytes.len()),
        _ => None,
    }
}

/// 给 UI 线程的发送端。
///
/// 接受语义(统一预算,三种状态明确区分):
/// - **已发送**:字节进入跨线程通道(中间态,不构成承诺);
/// - **已接受**:`send` 返回 true —— 预算已预留,字节保证按序
///   完整写入 PTY(实际写入、明确拒绝或会话结束才释放额度,
///   不因第二次内部检查丢失);
/// - **已写入**:字节实际写进 PTY master(会话侧按实际写入量
///   释放预算)。
#[derive(Clone)]
pub struct TermSender {
    tx: Sender<SessionCommand>,
    wake: Arc<UnixStream>,
    /// 在途输入字节(统一预算:跨线程消息 + PTY 待写队列):
    /// UI 入队即预留,会话按实际写入 / 发送失败 / 会话结束释放。
    inflight: Arc<std::sync::atomic::AtomicUsize>,
}

impl TermSender {
    /// 用户输入:原子检查 + 预留,超限**显式拒绝整块**(丢最新不丢
    /// 已排队;顺序由 mpsc FIFO 保证);返回 false 时调用方应给
    /// 用户可见反馈。控制命令不受预算影响。
    pub fn send(&self, msg: SessionCommand) -> bool {
        let Some(len) = input_len(&msg) else {
            if self.tx.send(msg).is_err() {
                return false;
            }
            let _ = (&*self.wake).write(&[0]);
            return true;
        };
        if !reserve_input(&self.inflight, len) {
            log::error!(
                "输入在途预算已满({}B),拒绝本次输入 {len}B(未截断,请重试)",
                MAX_INFLIGHT_INPUT_BYTES
            );
            return false;
        }
        if self.tx.send(msg).is_err() {
            // 会话已结束:释放预留,额度随会话退出统一清零。
            release_input(&self.inflight, len);
            return false;
        }
        // 唤醒 poll 循环。
        let _ = (&*self.wake).write(&[0]);
        true
    }

    /// 协议应答(终端 → PTY 自回环,如查询应答、OSC 52 确认):
    /// **必须送达** —— 丢弃应答会卡住应用;不受上限约束但计入
    /// 在途(会话侧按实际写入释放,记账不变;超出的少量字节会让
    /// 后续用户输入暂时被拒,直到写掉)。超过「上限 + 余量」
    /// 只告警(应用查询风暴的诊断信号)。
    pub fn send_response(&self, bytes: Vec<u8>) -> bool {
        let len = bytes.len();
        let total = self
            .inflight
            .fetch_add(len, std::sync::atomic::Ordering::Relaxed)
            + len;
        if !response_budget_ok(total) {
            log::error!(
                "协议应答在途 {total}B 超过余量上限 {}B(应用查询风暴?),仍送达不拒绝",
                MAX_INFLIGHT_INPUT_BYTES + MAX_RESPONSE_SLACK_BYTES
            );
        }
        if self.tx.send(SessionCommand::Input(bytes)).is_err() {
            release_input(&self.inflight, len);
            return false;
        }
        let _ = (&*self.wake).write(&[0]);
        true
    }
}

/// 拉起会话线程。**Term 的所有权移入本线程**(架构 §3):
/// UI 不再直接接触终端内核,经 SessionCommand 操作、
/// 读 SessionSnapshot 渲染。
pub fn spawn<U>(
    state: SessionState<U>,
    listener: U,
    pty: tty::Pty,
    snapshot_slot: Arc<Mutex<Option<SessionUpdate>>>,
) -> std::io::Result<TermSender>
where
    U: EventListener + Send + 'static,
{
    let (tx, rx) = mpsc::channel::<SessionCommand>();
    let (wake_tx, wake_rx) = UnixStream::pair()?;
    wake_tx.set_nonblocking(true)?;
    wake_rx.set_nonblocking(true)?;
    let inflight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let inflight_thread = inflight.clone();

    std::thread::Builder::new()
        .name("pty".into())
        .spawn(move || {
            run(
                state,
                listener,
                pty,
                rx,
                wake_rx,
                snapshot_slot,
                inflight_thread,
            );
        })
        .map_err(std::io::Error::other)?;

    Ok(TermSender {
        tx,
        wake: Arc::new(wake_tx),
        inflight,
    })
}

/// vte Handler 代理(P1 锚点修复):转发给 Term,同时精确计数
/// 「历史滚动量」(推入 scrollback 的行数)。网格行号在滚动时
/// 重编号(-1/行),累计滚动位移 cum_scroll 只增不减 ——
/// 稳定绝对行 = grid 行 + cum_scroll。
///
/// 计数规则(与 Term 的滚动语义对齐,只计「网格整体平移」):
/// - linefeed/newline:光标在滚动区域底行 → 滚动 +1(Term::linefeed
///   内部走 scroll_up,不经过 Handler::scroll_up);仅全屏区域
///   (0..screen)的滚动是整体平移,局部区域不计;
/// - input:DECAWM(LINE_WRAP)开启时,wrap_pending 挂起且触底 →
///   先 wrapline 滚动 +1;宽字符在倒数第二列即时 wrapline 且触底
///   → +1;DECAWM 关闭(CSI ?7l)时 Term::wrapline 直接返回,不换行
///   不滚动,不计;
/// - scroll_up(n)/delete_lines(n):全屏区域才计数,且按内核截断
///   (Term::scroll_up_relative 内 min(n, 区域高))→ +min(n, screen);
/// - scroll_down(n)/insert_blank_lines(n):全屏区域才计数,内容下移
///   n 行 → -n;n >= screen 时整个区域被清空(无平移)→ 0;
/// - wrap_pending 影子跨 advance 存活(由调用方传入 &mut),移动类
///   事件清除它(与 Term 的 input_needs_wrap 一致);
/// - scroll_region 影子同样跨 advance 存活:记录 CSI r 设置的区域
///   (start, end, 设置时行数),resize 会把区域重置为全屏(Term 内部),
///   由 region_is_full 按行数比对识别。
struct CountingHandler<'a, U: EventListener> {
    term: &'a mut Term<U>,
    /// 本次 advance 累计的历史滚动量。
    pub scrolled: i32,
    /// Term 的 input_needs_wrap 影子(跨 chunk 连续)。
    wrap_pending: &'a mut bool,
    /// Term 的 scroll_region 影子(跨 chunk 连续):0-based [start, end)
    /// 加设置时的行数;None = 默认全屏。
    scroll_region: &'a mut Option<(i32, i32, i32)>,
}

impl<'a, U: EventListener> CountingHandler<'a, U> {
    fn point(&self) -> Point {
        self.term.renderable_content().cursor.point
    }

    fn rows(&self) -> i32 {
        Dimensions::screen_lines(self.term.grid()) as i32
    }

    fn cols(&self) -> i32 {
        Dimensions::columns(self.term.grid()) as i32
    }

    /// 滚动区域是否为全屏(0..rows)。None 表示从未设置(默认全屏);
    /// Term 在 resize 时把滚动区域重置为全屏,所以影子记录的
    /// 设置时行数与当前不符也视为全屏,并顺手把影子归位。
    fn region_is_full(&mut self) -> bool {
        let rows = self.rows();
        if let Some((_, _, set_at)) = *self.scroll_region
            && set_at != rows
        {
            // resize 已把 Term 内部区域重置为全屏,影子归位。
            *self.scroll_region = None;
            return true;
        }
        match *self.scroll_region {
            None => true,
            Some((start, end, _)) => start == 0 && end == rows,
        }
    }
}

impl<U: EventListener> Handler for CountingHandler<'_, U> {
    fn set_title(&mut self, a1: Option<String>) {
        self.term.set_title(a1);
    }
    fn set_cursor_style(&mut self, a2: Option<CursorStyle>) {
        self.term.set_cursor_style(a2);
    }
    fn set_cursor_shape(&mut self, _shape: CursorShape) {
        self.term.set_cursor_shape(_shape);
    }
    fn input(&mut self, c: char) {
        use unicode_width::UnicodeWidthChar;
        let width = c.width().unwrap_or(0);
        let before = self.point();
        let cols = self.cols() as usize;
        let bottom = self.rows() - 1;
        if width == 0 {
            // 零宽字符不动光标,不影响 wrap 状态。
            self.term.input(c);
            return;
        }
        // wrap 判定(与 Term::input 语义对齐):
        // (a) 挂起(上一字符打印后越界);(b) 宽字符占位 wrap
        // (col == cols-1 处打印宽字符,Term 先占位换行)。
        // DECAWM(CSI ?7l)关闭时 Term::wrapline 直接返回,
        // 不换行、不滚动 —— 不产生计数。
        let wrap_on = self.term.mode().contains(TermMode::LINE_WRAP);
        let will_wrap =
            wrap_on && (*self.wrap_pending || (width == 2 && before.column.0 + 1 >= cols));
        // alt-screen 守卫:备用屏幕的滚动不移动主屏幕内容,
        // 不计入 cum_scroll(否则退出 vim 后主屏锚点整体偏移)。
        // 滚动区域守卫:只有全屏区域(0..screen)的滚动平移整个
        // 网格;局部区域滚动不产历史,不计。
        if will_wrap
            && before.line.0 == bottom
            && self.region_is_full()
            && !self.term.mode().contains(TermMode::ALT_SCREEN)
        {
            // wrapline 触底:内部 linefeed → scroll_up(历史 +1)。
            self.scrolled += 1;
        }
        self.term.input(c);
        // 挂起更新:wrap 后在行首打印;否则在原列。打印后越界 → 挂起。
        let printed_at = if will_wrap { 0 } else { before.column.0 };
        *self.wrap_pending = printed_at + width >= cols;
    }
    fn goto(&mut self, _line: i32, _col: usize) {
        *self.wrap_pending = false;
        self.term.goto(_line, _col);
    }
    fn goto_line(&mut self, _line: i32) {
        *self.wrap_pending = false;
        self.term.goto_line(_line);
    }
    fn goto_col(&mut self, _col: usize) {
        *self.wrap_pending = false;
        self.term.goto_col(_col);
    }
    fn insert_blank(&mut self, a3: usize) {
        self.term.insert_blank(a3);
    }
    fn move_up(&mut self, a4: usize) {
        *self.wrap_pending = false;
        self.term.move_up(a4);
    }
    fn move_down(&mut self, a5: usize) {
        *self.wrap_pending = false;
        self.term.move_down(a5);
    }
    fn identify_terminal(&mut self, _intermediate: Option<char>) {
        self.term.identify_terminal(_intermediate);
    }
    fn device_status(&mut self, a6: usize) {
        self.term.device_status(a6);
    }
    fn move_forward(&mut self, _col: usize) {
        *self.wrap_pending = false;
        self.term.move_forward(_col);
    }
    fn move_backward(&mut self, _col: usize) {
        *self.wrap_pending = false;
        self.term.move_backward(_col);
    }
    fn move_down_and_cr(&mut self, _row: usize) {
        *self.wrap_pending = false;
        self.term.move_down_and_cr(_row);
    }
    fn move_up_and_cr(&mut self, _row: usize) {
        *self.wrap_pending = false;
        self.term.move_up_and_cr(_row);
    }
    fn put_tab(&mut self, _count: u16) {
        *self.wrap_pending = false;
        self.term.put_tab(_count);
    }
    fn backspace(&mut self) {
        *self.wrap_pending = false;
        self.term.backspace();
    }
    fn carriage_return(&mut self) {
        *self.wrap_pending = false;
        self.term.carriage_return();
    }
    fn linefeed(&mut self) {
        // 光标已在滚动区域底行:换行触发滚动(Term 内部 scroll_up)。
        // alt-screen 守卫:备用屏幕的滚动不计入主屏累计位移。
        // 区域守卫:只有全屏区域的滚动是网格整体平移;局部区域内
        // 的滚动(光标在区域底行)或区域外(光标在屏幕底行但不属于
        // 区域)都不产历史,不计。
        if self.point().line.0 == self.rows() - 1
            && self.region_is_full()
            && !self.term.mode().contains(TermMode::ALT_SCREEN)
        {
            self.scrolled += 1;
        }
        self.term.linefeed();
    }
    fn bell(&mut self) {
        self.term.bell();
    }
    fn substitute(&mut self) {
        self.term.substitute();
    }
    fn newline(&mut self) {
        // 同 linefeed:光标在滚动区域底行且区域为全屏 → 滚动 +1。
        if self.point().line.0 == self.rows() - 1
            && self.region_is_full()
            && !self.term.mode().contains(TermMode::ALT_SCREEN)
        {
            self.scrolled += 1;
        }
        self.term.newline();
    }
    fn set_horizontal_tabstop(&mut self) {
        self.term.set_horizontal_tabstop();
    }
    fn scroll_up(&mut self, rows: usize) {
        // 只有全屏区域(0..screen)的滚动才是网格整体平移;
        // 局部滚动区内的 CSI S 只动区域内内容,历史不动,不计。
        // 内核把请求截断到区域高度(Term::scroll_up_relative 内
        // min(n, 区域高))—— 5 行窗口 CSI 999S 实际滚动 5 行。
        // alt-screen 守卫:备用屏幕滚动不计入主屏累计位移。
        let screen = self.rows() as usize;
        if self.region_is_full() && !self.term.mode().contains(TermMode::ALT_SCREEN) {
            self.scrolled += rows.min(screen) as i32;
        }
        self.term.scroll_up(rows);
    }
    fn scroll_down(&mut self, rows: usize) {
        // 内容整体下移 rows 行 → 锚点 -rows;rows >= screen 时
        // Term 把整个区域清空(不产生平移)→ 0。局部区域不计。
        let screen = self.rows() as usize;
        if self.region_is_full()
            && rows < screen
            && !self.term.mode().contains(TermMode::ALT_SCREEN)
        {
            self.scrolled -= rows as i32;
        }
        self.term.scroll_down(rows);
    }
    fn insert_blank_lines(&mut self, a9: usize) {
        // CSI Ps L:以光标行为 origin 的 scroll_down。全屏区域且
        // 光标在第 0 行时内容整体下移 → -n;n >= screen 时整个
        // 屏幕被清空(无平移)→ 0。其余情况(局部区域/光标不在
        // 顶部)不产历史,不计。
        let screen = self.rows() as usize;
        if self.region_is_full()
            && self.point().line.0 == 0
            && a9 < screen
            && !self.term.mode().contains(TermMode::ALT_SCREEN)
        {
            self.scrolled -= a9 as i32;
        }
        self.term.insert_blank_lines(a9);
    }
    fn delete_lines(&mut self, a10: usize) {
        // CSI Ps M:以光标行为 origin 的 scroll_up。全屏区域且
        // 光标在第 0 行时内容整体上移入历史 → +min(n, screen)。
        // 其余情况不产历史,不计。
        let screen = self.rows() as usize;
        if self.region_is_full()
            && self.point().line.0 == 0
            && !self.term.mode().contains(TermMode::ALT_SCREEN)
        {
            self.scrolled += a10.min(screen) as i32;
        }
        self.term.delete_lines(a10);
    }
    fn erase_chars(&mut self, a11: usize) {
        self.term.erase_chars(a11);
    }
    fn delete_chars(&mut self, a12: usize) {
        self.term.delete_chars(a12);
    }
    fn move_backward_tabs(&mut self, _count: u16) {
        *self.wrap_pending = false;
        self.term.move_backward_tabs(_count);
    }
    fn move_forward_tabs(&mut self, _count: u16) {
        *self.wrap_pending = false;
        self.term.move_forward_tabs(_count);
    }
    fn save_cursor_position(&mut self) {
        self.term.save_cursor_position();
    }
    fn restore_cursor_position(&mut self) {
        self.term.restore_cursor_position();
    }
    fn clear_line(&mut self, _mode: LineClearMode) {
        self.term.clear_line(_mode);
    }
    fn clear_screen(&mut self, _mode: ClearMode) {
        self.term.clear_screen(_mode);
    }
    fn clear_tabs(&mut self, _mode: TabulationClearMode) {
        self.term.clear_tabs(_mode);
    }
    fn reset_state(&mut self) {
        // RIS 把 Term 内部滚动区域重置为全屏,影子归位。
        *self.scroll_region = None;
        self.term.reset_state();
    }
    fn reverse_index(&mut self) {
        self.term.reverse_index();
    }
    fn terminal_attribute(&mut self, _attr: Attr) {
        self.term.terminal_attribute(_attr);
    }
    fn set_mode(&mut self, _mode: Mode) {
        self.term.set_mode(_mode);
    }
    fn unset_mode(&mut self, _mode: Mode) {
        self.term.unset_mode(_mode);
    }
    fn report_mode(&mut self, _mode: Mode) {
        self.term.report_mode(_mode);
    }
    fn set_private_mode(&mut self, _mode: PrivateMode) {
        self.term.set_private_mode(_mode);
    }
    fn unset_private_mode(&mut self, _mode: PrivateMode) {
        self.term.unset_private_mode(_mode);
    }
    fn report_private_mode(&mut self, _mode: PrivateMode) {
        self.term.report_private_mode(_mode);
    }
    fn set_scrolling_region(&mut self, _top: usize, _bottom: Option<usize>) {
        // 影子:CSI r 的参数是 1-based(top;bottom 含端点),Term 内部
        // 存为 0-based [top-1, bottom)。top >= bottom 的无效值被
        // Term 忽略(区域不变),影子同样保持不变。
        let bottom = _bottom.unwrap_or(self.rows() as usize);
        if _top < bottom {
            *self.scroll_region = Some((_top as i32 - 1, bottom as i32, self.rows()));
        }
        self.term.set_scrolling_region(_top, _bottom);
    }
    fn set_keypad_application_mode(&mut self) {
        self.term.set_keypad_application_mode();
    }
    fn unset_keypad_application_mode(&mut self) {
        self.term.unset_keypad_application_mode();
    }
    fn set_active_charset(&mut self, a13: CharsetIndex) {
        self.term.set_active_charset(a13);
    }
    fn configure_charset(&mut self, a14: CharsetIndex, a15: StandardCharset) {
        self.term.configure_charset(a14, a15);
    }
    fn set_color(&mut self, a16: usize, a17: Rgb) {
        self.term.set_color(a16, a17);
    }
    fn dynamic_color_sequence(&mut self, a18: String, a19: usize, a20: &str) {
        self.term.dynamic_color_sequence(a18, a19, a20);
    }
    fn reset_color(&mut self, a21: usize) {
        self.term.reset_color(a21);
    }
    fn clipboard_store(&mut self, a22: u8, a23: &[u8]) {
        self.term.clipboard_store(a22, a23);
    }
    fn clipboard_load(&mut self, a24: u8, a25: &str) {
        self.term.clipboard_load(a24, a25);
    }
    fn decaln(&mut self) {
        self.term.decaln();
    }
    fn push_title(&mut self) {
        self.term.push_title();
    }
    fn pop_title(&mut self) {
        self.term.pop_title();
    }
    fn text_area_size_pixels(&mut self) {
        self.term.text_area_size_pixels();
    }
    fn text_area_size_chars(&mut self) {
        self.term.text_area_size_chars();
    }
    fn set_hyperlink(&mut self, a26: Option<Hyperlink>) {
        self.term.set_hyperlink(a26);
    }
    fn report_keyboard_mode(&mut self) {
        self.term.report_keyboard_mode();
    }
    fn push_keyboard_mode(&mut self, _mode: KeyboardModes) {
        self.term.push_keyboard_mode(_mode);
    }
    fn pop_keyboard_modes(&mut self, _to_pop: u16) {
        self.term.pop_keyboard_modes(_to_pop);
    }
    fn set_keyboard_mode(&mut self, _mode: KeyboardModes, _behavior: KeyboardModesApplyBehavior) {
        self.term.set_keyboard_mode(_mode, _behavior);
    }
}

const PTY_KEY: usize = 1;
const WAKE_KEY: usize = 2;

/// 切换 PTY 兴趣集:writable 仅在有积压输入时监听 —— Level 模式下
/// 常可写会让 poll 立刻返回,造成热自旋,所以写空后要降级回 readable-only。
/// 注意必须用 `modify_with_mode`:`modify` 默认 Oneshot,事件只会触发一次。
unsafe fn set_pty_interest(
    poller: &Poller,
    pty: &tty::Pty,
    want_write: bool,
) -> std::io::Result<()> {
    let interest = if want_write {
        PollEvent::all(PTY_KEY)
    } else {
        PollEvent::readable(PTY_KEY)
    };
    poller.modify_with_mode(pty.file(), interest, polling::PollMode::Level)
}

/// 冲刷写队列:把 `pending` 尽量写进 `w`。
/// 返回 `(written, done)`:written = 本次**实际写入**的字节数
/// (中途 WouldBlock 时按已写入量返回,统一预算据此精确释放);
/// done = 队列已写空。
fn flush_pending<W: Write>(
    mut w: W,
    pending: &mut VecDeque<Vec<u8>>,
    woff: &mut usize,
    queued_bytes: &mut usize,
) -> std::io::Result<(usize, bool)> {
    let mut written = 0;
    while let Some(chunk) = pending.front() {
        match w.write(&chunk[*woff..]) {
            Ok(0) => return Ok((written, false)), // 罕见;视作缓冲满,等 writable。
            Ok(n) => {
                *woff += n;
                *queued_bytes -= n;
                written += n;
                if *woff >= chunk.len() {
                    pending.pop_front();
                    *woff = 0;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok((written, false)),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok((written, true))
}

/// 输入入队。统一预算已在发送端预留(覆盖跨线程消息 + 待写
/// 队列),此处**不再做第二次预算检查** —— 第二次拒绝会把已
/// 接受的输入整块丢弃(此前发送端 512 KiB / 会话端 256 KiB
/// 的不一致:300 KiB 粘贴在 send 成功后仍可能被拒)。pending
/// 总量由统一预留约束(用户输入 ≤ MAX;协议应答必须送达,
/// 可另计少量);超出 MAX 只告警、不拒绝。
fn enqueue_input(pending: &mut VecDeque<Vec<u8>>, queued_bytes: &mut usize, bytes: Vec<u8>) {
    if *queued_bytes + bytes.len() > MAX_INFLIGHT_INPUT_BYTES {
        log::warn!(
            "PTY 待写队列 {}B 超过统一预算 {}B(用户输入部分不应发生;协议应答另计)",
            *queued_bytes + bytes.len(),
            MAX_INFLIGHT_INPUT_BYTES
        );
    }
    *queued_bytes += bytes.len();
    pending.push_back(bytes);
}

fn drain_pty(
    poller: &Poller,
    pty: &tty::Pty,
    pending: &mut VecDeque<Vec<u8>>,
    woff: &mut usize,
    want_write: &mut bool,
    queued_bytes: &mut usize,
    inflight: &std::sync::atomic::AtomicUsize,
) -> bool {
    match flush_pending(pty.file(), pending, woff, queued_bytes) {
        Ok((written, true)) => {
            // 实际写入的字节释放统一预算(出队不释放 —— 额度
            // 只在写入 / 拒绝 / 会话结束时归还)。
            release_input(inflight, written);
            if *want_write {
                // 写空了:降级回 readable-only,避免 Level 模式热自旋。
                *want_write = false;
                if let Err(e) = unsafe { set_pty_interest(poller, pty, false) } {
                    log::error!("PTY 兴趣切换失败: {e}");
                    return false;
                }
            }
        }
        Ok((written, false)) => {
            // 部分写入或 WouldBlock:按已写入量释放,剩余留队。
            release_input(inflight, written);
            if !*want_write {
                // 缓冲满:升级监听 writable,等空间释放再冲刷。
                *want_write = true;
                if let Err(e) = unsafe { set_pty_interest(poller, pty, true) } {
                    log::error!("PTY 兴趣切换失败: {e}");
                    return false;
                }
            }
        }
        Err(e) => {
            log::error!("PTY 写入失败: {e}");
            return false;
        }
    }
    true
}

/// vi 增量搜索(会话侧):regex 命中后滚动定位;返回是否命中。
fn search<U: EventListener>(state: &mut SessionState<U>, query: &str, dir: i32) -> bool {
    use alacritty_terminal::term::search::RegexSearch;

    let Some(mut regex) = RegexSearch::new(query).ok() else {
        return false;
    };
    let cursor = state.term.renderable_content().cursor.point;
    let grid = state.term.grid();
    let history = (Dimensions::total_lines(grid) - Dimensions::screen_lines(grid)) as i32;
    let first = Point::new(Line(-history), Column(0));
    let last = Point::new(Line(Dimensions::screen_lines(grid) as i32 - 1), Column(0));
    let hit = if dir > 0 {
        state.term.regex_search_right(&mut regex, cursor, last)
    } else {
        state.term.regex_search_left(&mut regex, cursor, first)
    };
    if let Some(m) = hit {
        state.term.scroll_to_point(*m.start());
        true
    } else {
        false
    }
}

/// 应用一次 resize(PTY + 内核 + 快照,与 Resize 命令臂同一实现)。
fn apply_resize<U: EventListener>(
    cell: &mut (u32, u32),
    pty: &mut tty::Pty,
    state: &mut SessionState<U>,
    ws: alacritty_terminal::event::WindowSize,
    snapshot_slot: &Mutex<Option<SessionUpdate>>,
    listener: &U,
) {
    *cell = (ws.cell_width as u32, ws.cell_height as u32);
    pty.on_resize(ws);
    // 会话侧统一 resize(仅实际尺寸变化才失效语义状态)。
    state.resize(ws.num_cols as usize, ws.num_lines as usize);
    publish(snapshot_slot, state, listener);
}

/// 连续 Resize 合并(纯逻辑,与 run 排空循环同一实现,可单测):
/// `push(is_resize, ws)` 返回「处理该命令前必须应用的合并 resize」
/// (None = 无待应用);排空结束后调用 `drain()` 取尾段。合并只
/// 针对**连续**的 Resize —— 遇其它命令先应用,命令 FIFO 保持:
/// 后续复制/搜索/输入看到新布局与失效世代(Review 修复的 FIFO
/// resize 语义),连续的 configure 风暴只重排一次(第十二轮的
/// 合并优化)。
#[derive(Default)]
struct ResizeMerger {
    pending: Option<alacritty_terminal::event::WindowSize>,
}

impl ResizeMerger {
    fn new() -> Self {
        Self::default()
    }

    /// 推入一条命令:是 Resize → 只记最新值,返回 None;
    /// 是其它命令 → 返回此前积累的合并 resize(调用方先应用它
    /// 再处理该命令 —— 保 FIFO)。
    fn push(
        &mut self,
        is_resize: bool,
        ws: Option<alacritty_terminal::event::WindowSize>,
    ) -> Option<alacritty_terminal::event::WindowSize> {
        if is_resize {
            self.pending = ws;
            None
        } else {
            self.pending.take()
        }
    }

    /// 排空结束:取走尾段合并 resize。
    fn drain(&mut self) -> Option<alacritty_terminal::event::WindowSize> {
        self.pending.take()
    }
}

fn run<U>(
    mut state: SessionState<U>,
    listener: U,
    mut pty: tty::Pty,
    rx: Receiver<SessionCommand>,
    mut wake_rx: UnixStream,
    snapshot_slot: Arc<Mutex<Option<SessionUpdate>>>,
    inflight: Arc<std::sync::atomic::AtomicUsize>,
) where
    U: EventListener,
{
    let poller = match Poller::new() {
        Ok(p) => p,
        Err(e) => {
            log::error!("PTY poller 创建失败: {e}");
            return;
        }
    };
    // PTY 读取源(与原版一样注册 master fd)。
    unsafe {
        if poller
            .add_with_mode(
                pty.file(),
                PollEvent::readable(PTY_KEY),
                polling::PollMode::Level,
            )
            .is_err()
        {
            log::error!("PTY fd 注册失败");
            return;
        }
        if poller
            .add_with_mode(
                &wake_rx,
                PollEvent::readable(WAKE_KEY),
                polling::PollMode::Level,
            )
            .is_err()
        {
            log::error!("唤醒管道注册失败");
            return;
        }
    }

    let mut parser: ansi::Processor = ansi::Processor::new();
    let mut cell: (u32, u32) = (8, 16);
    let mut scanner = StreamScanner::default();
    let mut buf = vec![0u8; 16 * 1024];
    let mut events = polling::Events::new();
    let mut exiting = false;
    // 累计滚动位移在 SessionState::meta(与 Term 同锁,架构 §3):
    // 稳定绝对行 = grid 行 + cum_scroll,只增不减,CountingHandler 精确计数。
    // Term 的 input_needs_wrap 影子(跨 chunk 连续,CountingHandler 维护)。
    let mut wrap_pending = false;
    // Term 的 scroll_region 影子(跨 chunk 连续,CountingHandler 维护)。
    let mut scroll_region: Option<(i32, i32, i32)> = None;
    // PTY 写队列:master fd 非阻塞,缓冲满时 write 返回 WouldBlock ——
    // 输入不能丢,入队等 writable(见 drain_pty)。
    let mut pending: VecDeque<Vec<u8>> = VecDeque::new();
    let mut woff: usize = 0;
    let mut want_write = false;
    // 写队列在途字节数(与 pending 内容同步维护,预算用)。
    let mut queued_bytes: usize = 0;
    // 备用屏幕状态跟踪(坐标身份切换 → 推进锚点世代)。
    let mut alt_screen_active = false;

    loop {
        // DEC 2026 同步超时:即便无数据也要按时推进 parser(与原版同款)。
        let handler = parser.sync_timeout();
        let timeout = handler
            .sync_timeout()
            .map(|st| st.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::from_millis(500))
            .min(Duration::from_millis(500));

        events.clear();
        if let Err(e) = poller.wait(&mut events, Some(timeout)) {
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            log::error!("PTY poll 失败: {e}");
            break;
        }

        for ev in events.iter() {
            match ev.key {
                PTY_KEY => {
                    let mut read_any = false;
                    loop {
                        match pty.file().read(&mut buf) {
                            Ok(0) => {
                                exiting = true;
                                break;
                            }
                            Ok(n) => {
                                read_any = true;
                                let chunk = &buf[..n];
                                for ev in scanner.scan(chunk) {
                                    match ev {
                                        StreamEvent::Text(bytes) => {
                                            let mut h = CountingHandler {
                                                term: &mut state.term,
                                                scrolled: 0,
                                                wrap_pending: &mut wrap_pending,
                                                scroll_region: &mut scroll_region,
                                            };
                                            parser.advance(&mut h, &bytes);
                                            // Term 归本线程所有:网格与锚点
                                            // 天然同一版本(架构 §3)。
                                            state.meta.cum_scroll += h.scrolled;
                                            state.meta.revision.0 += 1;
                                            // 主/备屏幕切换 = 坐标身份切换:
                                            // 推进锚点世代 —— 跨屏幕的锚点
                                            // 请求不得混用坐标(主屏网格在
                                            // alt-screen 期间冻结,块/图像锚点
                                            // 本身不受影响,不清空)。
                                            let alt =
                                                state.term.mode().contains(TermMode::ALT_SCREEN);
                                            if alt != alt_screen_active {
                                                alt_screen_active = alt;
                                                state.advance_epoch();
                                                log::debug!(
                                                    "备用屏幕 {}:锚点世代 {}",
                                                    if alt { "进入" } else { "退出" },
                                                    state.meta.invalidate_epoch.0
                                                );
                                            }
                                        }
                                        // P3b:alt-screen 守卫 —— vim/less/tmux 等全屏
                                        // 应用的 OSC 133 不产生命令块(行号语义
                                        // 属于应用自身,且退出 alt-screen 后行
                                        // 重排会破坏锚点)。
                                        StreamEvent::PromptStart
                                        | StreamEvent::OutputStart
                                        | StreamEvent::Exit(_)
                                            if state.term.mode().contains(TermMode::ALT_SCREEN) =>
                                        {
                                            // 抑制。
                                        }
                                        StreamEvent::PromptStart => {
                                            let (line, col) = {
                                                let c =
                                                    state.term.renderable_content().cursor.point;
                                                (
                                                    c.line.0 + state.meta.cum_scroll,
                                                    c.column.0 as u32,
                                                )
                                            };
                                            state
                                                .apply_event(BlockEvent::PromptStart { line, col });
                                        }
                                        StreamEvent::OutputStart => {
                                            let line = {
                                                state.term.renderable_content().cursor.point.line.0
                                                    + state.meta.cum_scroll
                                            };
                                            state.apply_event(BlockEvent::OutputStart(line));
                                        }
                                        StreamEvent::Exit(code) => {
                                            state.apply_event(BlockEvent::Exit { code });
                                        }
                                        StreamEvent::ClearScreen => {
                                            state.apply_event(BlockEvent::Invalidated);
                                        }
                                        StreamEvent::KittyResponse(bytes) => {
                                            // 查询/放置应答:写回 PTY(进写队列,
                                            // 缓冲满不丢);应答很小,必须送达
                                            // —— 不计上限但计入在途(写入后
                                            // 随 flush 释放,记账不变;丢弃
                                            // 协议应答会卡住应用)。超余量只
                                            // 告警(查询风暴诊断)。
                                            let total = inflight.fetch_add(
                                                bytes.len(),
                                                std::sync::atomic::Ordering::Relaxed,
                                            ) + bytes.len();
                                            if !response_budget_ok(total) {
                                                log::error!(
                                                    "协议应答在途 {total}B 超过余量上限 {}B(应用查询风暴?),仍送达不拒绝",
                                                    MAX_INFLIGHT_INPUT_BYTES
                                                        + MAX_RESPONSE_SLACK_BYTES
                                                );
                                            }
                                            queued_bytes += bytes.len();
                                            pending.push_back(bytes);
                                            if !drain_pty(
                                                &poller,
                                                &pty,
                                                &mut pending,
                                                &mut woff,
                                                &mut want_write,
                                                &mut queued_bytes,
                                                &inflight,
                                            ) {
                                                exiting = true;
                                            }
                                        }
                                        StreamEvent::Kitty(img) => {
                                            // 放置位置 = 当前光标(流序保证正确)。
                                            let (line, col) = {
                                                let content = state.term.renderable_content();
                                                (
                                                    content.cursor.point.line.0
                                                        + state.meta.cum_scroll,
                                                    content.cursor.point.column.0 as u32,
                                                )
                                            };
                                            // 光标移动(kitty C=1 / sixel 下移,iTerm2 右移)。
                                            if img.action != crate::kitty::KittyAction::Delete {
                                                let move_seq = match img.cursor_after {
                                                    crate::kitty::CursorAfter::None => None,
                                                    // 协议默认:右移 cols、下移 rows。
                                                    crate::kitty::CursorAfter::Move => {
                                                        let cols = (img.w as f32
                                                            / cell.0.max(1) as f32)
                                                            .ceil()
                                                            .max(1.0)
                                                            as u32;
                                                        let rows = (img.h as f32
                                                            / cell.1.max(1) as f32)
                                                            .ceil()
                                                            .max(1.0)
                                                            as u32;
                                                        Some(format!("\x1b[{cols}C\x1b[{rows}B"))
                                                    }
                                                    crate::kitty::CursorAfter::Down => {
                                                        let rows = (img.h as f32
                                                            / cell.1.max(1) as f32)
                                                            .ceil()
                                                            .max(1.0)
                                                            as u32;
                                                        Some(format!("\r\x1b[{rows}B"))
                                                    }
                                                    crate::kitty::CursorAfter::Right => {
                                                        let cols = (img.w as f32
                                                            / cell.0.max(1) as f32)
                                                            .ceil()
                                                            .max(1.0)
                                                            as u32;
                                                        Some(format!("\x1b[{cols}C"))
                                                    }
                                                };
                                                if let Some(seq) = move_seq {
                                                    let mut h = CountingHandler {
                                                        term: &mut state.term,
                                                        scrolled: 0,
                                                        wrap_pending: &mut wrap_pending,
                                                        scroll_region: &mut scroll_region,
                                                    };
                                                    parser.advance(&mut h, seq.as_bytes());
                                                    // 移动不滚动(scrolled 恒为 0),
                                                    // 版本照进。
                                                    state.meta.cum_scroll += h.scrolled;
                                                    state.meta.revision.0 += 1;
                                                }
                                            }
                                            state.apply_event(BlockEvent::Image { img, line, col });
                                        }
                                    }
                                }
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                            Err(e) => {
                                log::error!("PTY 读取失败: {e}");
                                exiting = true;
                                break;
                            }
                        }
                    }
                    // 每波输出后发布最新快照并唤醒 UI(架构 §3):
                    // 块事件已入通道、快照已入单槽 —— 不主动唤醒的话,
                    // 后续突发的语义事件会一直躺在通道里(实测:TAY_EXEC
                    // 只写一次 wake 管道,第二波 133 标记永不送达)。
                    if read_any {
                        publish(&snapshot_slot, &state, &listener);
                    }
                    // writable:冲刷积压输入(只有在 want_write 时才可能)。
                    if ev.writable
                        && !pending.is_empty()
                        && !drain_pty(
                            &poller,
                            &pty,
                            &mut pending,
                            &mut woff,
                            &mut want_write,
                            &mut queued_bytes,
                            &inflight,
                        )
                    {
                        exiting = true;
                    }
                }
                WAKE_KEY => {
                    // 排空唤醒字节;消息统一在事件之后处理。
                    let mut sink = [0u8; 64];
                    while wake_rx.read(&mut sink).is_ok() {}
                }
                _ => {}
            }
        }

        // UI 消息:事件之后统一处理(不再依赖 WAKE_KEY 分支,避免
        // `rx.try_recv()` 探测性检查吞掉刚到的消息)。
        let mut handled_msg = false;
        // 连续 Resize 合并(ResizeMerger,纯逻辑见单测):只保留
        // 最新值,遇其它命令或排空结束时应用 —— 命令 FIFO 保持
        // (后续复制/搜索/输入看到新布局与失效世代),连续的
        // configure 风暴只重排一次。
        let mut merger = ResizeMerger::new();
        loop {
            match rx.try_recv() {
                Ok(msg) => {
                    handled_msg = true;
                    let is_resize = matches!(msg, SessionCommand::Resize(_));
                    let resize = match &msg {
                        SessionCommand::Resize(ws) => Some(*ws),
                        _ => None,
                    };
                    if let Some(ws) = merger.push(is_resize, resize) {
                        apply_resize(
                            &mut cell,
                            &mut pty,
                            &mut state,
                            ws,
                            &snapshot_slot,
                            &listener,
                        );
                    }
                    match msg {
                        SessionCommand::Input(bytes) => {
                            // 出队**不**释放预算:统一预算覆盖跨线程
                            // 消息与 PTY 待写数据,额度只在字节实际
                            // 写入(flush_pending)、发送失败(发送端)
                            // 或会话结束时释放 —— 已接受的输入不因
                            // 第二次预算检查丢失(入队不再拒绝)。
                            enqueue_input(&mut pending, &mut queued_bytes, bytes);
                            if !drain_pty(
                                &poller,
                                &pty,
                                &mut pending,
                                &mut woff,
                                &mut want_write,
                                &mut queued_bytes,
                                &inflight,
                            ) {
                                exiting = true;
                            }
                        }
                        SessionCommand::Resize(_) => {
                            // 已并入 merger,此处只占位。
                        }
                        SessionCommand::ScrollDisplay(scroll) => {
                            state.term.scroll_display(scroll);
                            state.bump();
                            publish(&snapshot_slot, &state, &listener);
                        }
                        SessionCommand::ScrollToAnchor { anchor, epoch } => {
                            // 世代校验:来源快照已失效(清屏/resize/
                            // 屏幕切换)→ no-op,不猜位置。
                            if state.scroll_to_anchor(anchor, epoch) {
                                state.bump();
                            }
                            publish(&snapshot_slot, &state, &listener);
                        }
                        SessionCommand::ToggleVi => {
                            state.term.toggle_vi_mode();
                            state.bump();
                            publish(&snapshot_slot, &state, &listener);
                        }
                        SessionCommand::ViMotion(m) => {
                            state.term.vi_motion(m);
                            state.bump();
                            publish(&snapshot_slot, &state, &listener);
                        }
                        SessionCommand::Search { query, dir, reply } => {
                            let found = search(&mut state, &query, dir);
                            let revision = state.meta.revision;
                            state.bump();
                            publish(&snapshot_slot, &state, &listener);
                            let _ = reply.send(SessionReply::Search { found, revision });
                        }
                        SessionCommand::CopyAnchors {
                            from_line,
                            from_col,
                            to_line,
                            to_col,
                            epoch,
                            reply,
                        } => {
                            // 执行前世代校验:不匹配 → 明确返回过期
                            // (Stale),不猜测坐标;世代匹配但锚点被
                            // 淘汰 → Evicted(text = None 且非 stale)。
                            let (text, stale) = match state
                                .copy_anchors_checked(from_line, from_col, to_line, to_col, epoch)
                            {
                                crate::session::CopyOutcome::Ok(t) => (Some(t), false),
                                crate::session::CopyOutcome::Stale => {
                                    log::info!("复制请求世代过期(清屏/resize/屏幕切换),已拒绝");
                                    (None, true)
                                }
                                crate::session::CopyOutcome::Evicted => (None, false),
                            };
                            let _ = reply.send(SessionReply::Copy {
                                text,
                                revision: state.meta.revision,
                                stale,
                            });
                        }
                        SessionCommand::SelectionToString { reply } => {
                            let text = state.term.selection_to_string();
                            let _ = reply.send(SessionReply::Selection {
                                text,
                                revision: state.meta.revision,
                            });
                        }
                        SessionCommand::Shutdown => {
                            exiting = true;
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    // UI 端消失且未发 Shutdown:继续跑已无意义。
                    exiting = true;
                    break;
                }
            }
        }

        // 排空结束时应用尾段的合并 resize(原「合并为最新值」语义:
        // 一轮排空内多个 Resize 只应用最后一个)。
        if let Some(ws) = merger.drain() {
            apply_resize(
                &mut cell,
                &mut pty,
                &mut state,
                ws,
                &snapshot_slot,
                &listener,
            );
        }

        // 同步超时推进(无数据且无消息;原版语义)。
        if events.is_empty() && !handled_msg {
            parser.stop_sync(&mut state.term);
            publish(&snapshot_slot, &state, &listener);
        }

        // 子进程退出。
        if let Some(ChildEvent::Exited(status)) = pty.next_child_event() {
            if let Some(status) = status {
                listener.send_event(Event::ChildExit(status));
            }
            break;
        }
        if exiting {
            break;
        }
    }
    // 会话结束:释放全部在途额度(待写队列与仍在通道中的消息
    // 一并作废;并发的发送端释放用饱和减,不会下溢)。
    inflight.store(0, std::sync::atomic::Ordering::Relaxed);
    log::debug!("PTY 事件循环退出");
}

/// 字节流事件(扫描器输出;term 无关,可单测)。
#[derive(Debug)]
enum StreamEvent {
    /// 喂给终端的字节。
    Text(Vec<u8>),
    /// kitty graphics 图像(已解码;放置位置由调用方按当前光标决定)。
    Kitty(crate::kitty::KittyImage),
    /// OSC 133 提示符开始。
    PromptStart,
    /// OSC 133 命令输出开始(P3b:C 边界)。
    OutputStart,
    /// OSC 133 退出码。
    Exit(i32),
    /// 清屏/重置序列(ED2 `\x1b[2J` / 清历史 `\x1b[3J` / RIS `\x1bc`):
    /// 行号重排,块与图像锚点失效。
    ClearScreen,
    /// kitty 查询/放置应答:字节要经 PTY master 写回客户端程序
    /// (终端的响应通道就是 PTY 输入方向)。
    KittyResponse(Vec<u8>),
}

#[derive(Default)]
struct ClearDetector {
    cleared: bool,
}

impl alacritty_terminal::vte::Perform for ClearDetector {
    fn csi_dispatch(
        &mut self,
        params: &alacritty_terminal::vte::Params,
        intermediates: &[u8],
        ignore: bool,
        action: char,
    ) {
        if !ignore && intermediates.is_empty() && action == 'J' {
            let mode = params.iter().next().and_then(|p| p.first()).copied();
            self.cleared |= matches!(mode, Some(2 | 3));
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        self.cleared |= !ignore && intermediates.is_empty() && byte == b'c';
    }
}

/// 文本是否以未完成的转义序列结尾(clear_parser 快速路径判定):
/// 孤立 ESC、或 CSI(ESC [)后未见终止字节(final byte 0x40-0x7E)。
/// 保守实现:误报(true)只多跑一次检测;漏报会漏清屏 —— 不允许。
fn text_ends_in_escape_sequence(text: &[u8]) -> bool {
    let Some(i) = text.iter().rposition(|&b| b == 0x1b) else {
        return false; // 无 ESC:不可能打开序列。
    };
    let rest = &text[i + 1..];
    if rest.is_empty() {
        return true; // 孤立 ESC:下一字节决定序列类型。
    }
    if rest[0] != b'[' {
        return false; // 其它 ESC 序列 = ESC + 单字节,已完整。
    }
    // CSI:参数 0x30-0x3F、中间 0x20-0x2F,终止字节 0x40-0x7E。
    !rest.iter().skip(1).any(|&b| (0x40..=0x7e).contains(&b))
}

/// 流扫描器:字节 → 事件(OSC 133 命令块 + APC kitty graphics)。
#[derive(Default)]
struct StreamScanner {
    /// 待发文本缓冲。
    text: Vec<u8>,
    /// 与内核相同的 VT 状态机跨 flush 保留状态，避免分段清屏漏检。
    clear_parser: alacritty_terminal::vte::Parser,
    /// 上一次 flush 的文本以未完成的转义序列结尾(clear_parser 有
    /// 跨 flush 待决状态)—— 快速路径判定:为 false 且本次文本
    /// 无 ESC 时才跳过检测。
    clear_seq_open: bool,
    /// ESC ] 收集中(OSC)。
    osc: Vec<u8>,
    /// ESC _ 收集中(APC)。
    apc: Vec<u8>,
    /// ESC P 收集中(DCS/sixel)。
    dcs: Vec<u8>,
    /// StEsc 的来源(APC 或 DCS)。
    esc_origin: EscOrigin,
    state: ScanState,
    /// 上一 chunk 末尾的孤立 ESC(Text/Osc 状态下跨 chunk 配对)。
    esc_pending: bool,
    /// kitty m=1 分块传输的在途重组(P3a):首块控制键 + 已收到的载荷。
    kitty_partial: Option<KittyChunk>,
    /// kitty 传输缓存:已传输未删除的图像(id → 图像),a=p 放置用。
    kitty_cache: Vec<(u32, crate::kitty::KittyImage)>,
}

/// kitty 分块重组状态:首块控制键 + 原始载荷(不按块解码 ——
/// base64 等编码可能跨块切分)。
struct KittyChunk {
    /// 首块控制键(不含 "G" 与 ";" 分隔符,含全部参数)。
    ctl: Vec<u8>,
    /// 累计原始载荷(从首块起)。
    raw: Vec<u8>,
}

/// kitty 分块图像累计载荷上限:超出即丢弃(防内存失控)。
const MAX_KITTY_CHUNK_BYTES: usize = 32 * 1024 * 1024;

/// kitty 传输缓存字节预算(a=t/a=T 累计像素,超限丢最旧)。
const MAX_KITTY_CACHE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Default, Clone, Copy, PartialEq)]
enum EscOrigin {
    Apc,
    #[default]
    Dcs,
}

#[derive(Default, PartialEq)]
enum ScanState {
    #[default]
    Text,
    Osc,
    Apc,
    Dcs,
    /// ST(ESC \)的 ESC 已见。
    StEsc,
}

impl StreamScanner {
    fn scan(&mut self, chunk: &[u8]) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < chunk.len() {
            let c = chunk[i];
            match self.state {
                ScanState::Text => {
                    // 上一 chunk 末尾的孤立 ESC 在此配对。
                    if self.esc_pending {
                        self.esc_pending = false;
                        match c {
                            b']' => {
                                self.state = ScanState::Osc;
                                self.osc.clear();
                                i += 1;
                                continue;
                            }
                            b'_' => {
                                self.state = ScanState::Apc;
                                self.apc.clear();
                                i += 1;
                                continue;
                            }
                            b'P' => {
                                self.state = ScanState::Dcs;
                                self.dcs.clear();
                                i += 1;
                                continue;
                            }
                            // 不是序列:ESC 按普通字节处理,继续处理 c。
                            _ => self.text.push(0x1b),
                        }
                    }
                    if c == 0x1b {
                        if i + 1 < chunk.len() {
                            match chunk[i + 1] {
                                b']' => {
                                    self.state = ScanState::Osc;
                                    self.osc.clear();
                                    i += 2;
                                    continue;
                                }
                                b'_' => {
                                    self.state = ScanState::Apc;
                                    self.apc.clear();
                                    i += 2;
                                    continue;
                                }
                                b'P' => {
                                    self.state = ScanState::Dcs;
                                    self.dcs.clear();
                                    i += 2;
                                    continue;
                                }
                                _ => {
                                    self.text.push(0x1b);
                                    i += 1;
                                    continue;
                                }
                            }
                        }
                        // 孤立 ESC(如 "…\x1b" + "]133;A…" 跨 chunk):
                        // 挂起,待下一 chunk 配对。
                        self.esc_pending = true;
                        i += 1;
                        continue;
                    }
                    self.text.push(c);
                    i += 1;
                }
                ScanState::Osc => {
                    // 上一 chunk 末尾的 ESC:在此判定 ST 还是内容。
                    if self.esc_pending {
                        self.esc_pending = false;
                        if c == b'\\' {
                            self.finish_osc(&mut out, true);
                            self.state = ScanState::Text;
                            i += 1;
                            continue;
                        }
                        self.osc.push(0x1b);
                    }
                    if c == 0x07 {
                        self.finish_osc(&mut out, false);
                        self.state = ScanState::Text;
                        i += 1;
                    } else if c == 0x1b {
                        if i + 1 < chunk.len() {
                            if chunk[i + 1] == b'\\' {
                                self.finish_osc(&mut out, true);
                                self.state = ScanState::Text;
                                i += 2;
                            } else {
                                self.osc.push(0x1b);
                                i += 1;
                            }
                        } else {
                            // 孤立 ESC:下一 chunk 判定 ST 还是内容。
                            self.esc_pending = true;
                            i += 1;
                        }
                    } else {
                        self.osc.push(c);
                        i += 1;
                    }
                }
                ScanState::Apc => {
                    if c == 0x1b {
                        self.state = ScanState::StEsc;
                        self.esc_origin = EscOrigin::Apc;
                        i += 1;
                    } else {
                        self.apc.push(c);
                        i += 1;
                    }
                }
                ScanState::Dcs => {
                    if c == 0x1b {
                        self.state = ScanState::StEsc;
                        self.esc_origin = EscOrigin::Dcs;
                        i += 1;
                    } else {
                        self.dcs.push(c);
                        i += 1;
                    }
                }
                ScanState::StEsc => {
                    if c == b'\\' {
                        // APC / DCS 结束。
                        match self.esc_origin {
                            EscOrigin::Apc => self.finish_apc(&mut out),
                            EscOrigin::Dcs => self.finish_dcs(&mut out),
                        }
                        self.state = ScanState::Text;
                        i += 1;
                    } else {
                        // 不是 ST:ESC 属于内容。
                        match self.esc_origin {
                            EscOrigin::Apc => {
                                self.apc.push(0x1b);
                                self.apc.push(c);
                                self.state = ScanState::Apc;
                            }
                            EscOrigin::Dcs => {
                                self.dcs.push(0x1b);
                                self.dcs.push(c);
                                self.state = ScanState::Dcs;
                            }
                        }
                        i += 1;
                    }
                }
            }
        }
        // 防呆:序列过长 = 异常输出,原样转发而不是无提示丢弃 ——
        // 转发前缀后,剩余字节(含终止符)会作为普通文本流过,终端仍能
        // 完整消费这条序列,只是我们不再解析它。
        if self.osc.len() > 512 {
            if self.esc_pending {
                self.osc.push(0x1b);
                self.esc_pending = false;
            }
            self.text.extend_from_slice(b"\x1b]");
            self.text.append(&mut self.osc);
            self.state = ScanState::Text;
        }
        if self.apc.len() > 64 * 1024 * 1024 {
            self.text.extend_from_slice(b"\x1b_");
            self.text.append(&mut self.apc);
            self.state = ScanState::Text;
        }
        if self.dcs.len() > 64 * 1024 * 1024 {
            self.text.extend_from_slice(b"\x1bP");
            self.text.append(&mut self.dcs);
            self.state = ScanState::Text;
        }
        self.flush_text(&mut out);
        out
    }

    fn finish_apc(&mut self, out: &mut Vec<StreamEvent>) {
        match crate::kitty::parse_apc(&self.apc) {
            crate::kitty::Apc::Kitty(img) => {
                self.flush_text(out);
                self.handle_kitty_chunk(img, out);
            }
            crate::kitty::Apc::Query { id, action } => {
                // 协议要求查询/放置必须应答(经 PTY 写给客户端程序):
                // q → OK(声明支持);p → ENOENT(无传输缓存,诚实拒绝)。
                // q=2 静默键未实现:始终应答(记录于文档)。
                self.flush_text(out);
                let resp = match action {
                    'q' => format!("\x1b_Gi={id},OK\x1b\\").into_bytes(),
                    _ => format!("\x1b_Gi={id},ENOENT: no such image\x1b\\").into_bytes(),
                };
                out.push(StreamEvent::KittyResponse(resp));
            }
            crate::kitty::Apc::Passthrough(bytes) => {
                // 未知 APC:原样给终端(终端自行忽略)。
                // 在途分块保留(协议要求块间连续;宽容处理)。
                self.text.extend_from_slice(b"\x1b_");
                self.text.extend_from_slice(&bytes);
                self.text.extend_from_slice(b"\x1b\\");
            }
            crate::kitty::Apc::Invalid => {
                // 畸形 kitty 序列丢弃;在途分块一并作废。
                self.kitty_partial = None;
            }
        }
    }

    /// kitty m=1 分块重组(P3a):首块携带控制键,后续块仅追加载荷,
    /// 载荷不按块解码(base64 等编码可跨块切分),m=0 末块到达后拼接
    /// 成完整序列统一解析;累计超 32 MiB 丢弃;流中断(无末块)时静默
    /// 作废 —— 绝不产出半幅图像。无 m 键的普通传输是独立完整图像,
    /// 会作废在途分块。
    /// 按 a 键动作分派完整图像:
    /// - TransmitDisplay:进传输缓存并显示;
    /// - TransmitOnly(a=t):只进缓存,不显示;
    /// - Place(a=p):查缓存放置,命中回 OK、未命中回 ENOENT
    ///   (应答经 PTY 写回客户端;放置序列的 C 键语义以本次为准)。
    fn dispatch_kitty(&mut self, img: crate::kitty::KittyImage, out: &mut Vec<StreamEvent>) {
        use crate::kitty::KittyAction::*;
        match img.action {
            Delete => unreachable!("删除已在 handle_kitty_chunk 处理"),
            TransmitOnly => self.cache_store(img),
            TransmitDisplay => {
                self.cache_store(img.clone());
                out.push(StreamEvent::Kitty(img));
            }
            Place => match self.cache_lookup(img.id) {
                Some(mut cached) => {
                    cached.cursor_after = img.cursor_after;
                    cached.more = None;
                    cached.action = TransmitDisplay;
                    let resp = format!("\x1b_Gi={},OK\x1b\\", img.id).into_bytes();
                    out.push(StreamEvent::Kitty(cached));
                    out.push(StreamEvent::KittyResponse(resp));
                }
                None => {
                    let resp =
                        format!("\x1b_Gi={},ENOENT: no such image\x1b\\", img.id).into_bytes();
                    out.push(StreamEvent::KittyResponse(resp));
                }
            },
        }
    }

    /// 传输缓存写入:同 id 覆盖;字节预算超限丢最旧。
    fn cache_store(&mut self, img: crate::kitty::KittyImage) {
        self.kitty_cache.retain(|(i, _)| *i != img.id);
        self.kitty_cache.push((img.id, img));
        let mut total: usize = self.kitty_cache.iter().map(|(_, i)| i.rgba.len()).sum();
        while total > MAX_KITTY_CACHE_BYTES && self.kitty_cache.len() > 1 {
            let drop_bytes = self
                .kitty_cache
                .first()
                .map(|(_, i)| i.rgba.len())
                .unwrap_or(0);
            total -= drop_bytes;
            self.kitty_cache.remove(0);
        }
    }

    fn cache_lookup(&self, id: u32) -> Option<crate::kitty::KittyImage> {
        self.kitty_cache
            .iter()
            .find(|(i, _)| *i == id)
            .map(|(_, img)| img.clone())
    }

    fn handle_kitty_chunk(
        &mut self,
        mut img: crate::kitty::KittyImage,
        out: &mut Vec<StreamEvent>,
    ) {
        // 删除动作与分块无关,立即生效。
        if img.action == crate::kitty::KittyAction::Delete {
            self.kitty_partial = None;
            // 删除同步清传输缓存(id=0 = 全部)。
            if img.id == 0 {
                self.kitty_cache.clear();
            } else {
                self.kitty_cache.retain(|(i, _)| *i != img.id);
            }
            out.push(StreamEvent::Kitty(img));
            return;
        }
        match img.more {
            // 无 m 键:独立完整传输(parse_apc 已解码);在途半幅作废。
            None => {
                self.kitty_partial = None;
                self.dispatch_kitty(img, out);
            }
            // m=0:末块 —— 拼接收尾,重组完整序列后统一解析。
            Some(false) => {
                // 分块载荷出句柄(分块中间态本身是暂存,复制可接受;
                // 最终图像解码时再建共享句柄)。
                let mut payload = std::mem::take(&mut img.rgba).to_vec();
                let Some(mut part) = self.kitty_partial.take() else {
                    // 孤立的 m=0 块:无首块控制键,无法重组,丢弃。
                    return;
                };
                if part.raw.len() + payload.len() > MAX_KITTY_CHUNK_BYTES {
                    log::warn!(
                        "kitty 分块图像超出 {} MiB 预算,丢弃",
                        MAX_KITTY_CHUNK_BYTES >> 20
                    );
                    return;
                }
                part.raw.append(&mut payload);
                // 重组:首块控制键(去掉 m 键)+ 完整原始载荷 → 完整解析。
                let ctl: Vec<u8> = part
                    .ctl
                    .split(|&b| b == b',')
                    .filter(|kv| !kv.starts_with(b"m=") && *kv != b"m")
                    .collect::<Vec<_>>()
                    .join(&b',');
                let mut synthetic = b"G".to_vec();
                synthetic.extend_from_slice(&ctl);
                synthetic.push(b';');
                synthetic.extend_from_slice(&part.raw);
                match crate::kitty::parse_apc(&synthetic) {
                    crate::kitty::Apc::Kitty(full) => self.dispatch_kitty(full, out),
                    _ => log::warn!("kitty 分块重组失败,丢弃"),
                }
            }
            // m=1:开启或延续分块。后续块的控制键(除 m)按协议忽略。
            Some(true) => {
                // 分块载荷出句柄(分块中间态本身是暂存,复制可接受;
                // 最终图像解码时再建共享句柄)。
                let mut payload = std::mem::take(&mut img.rgba).to_vec();
                match self.kitty_partial.as_mut() {
                    Some(part) => {
                        if part.raw.len() + payload.len() > MAX_KITTY_CHUNK_BYTES {
                            self.kitty_partial = None;
                            log::warn!(
                                "kitty 分块图像超出 {} MiB 预算,丢弃",
                                MAX_KITTY_CHUNK_BYTES >> 20
                            );
                            return;
                        }
                        part.raw.append(&mut payload);
                    }
                    None => {
                        if payload.len() > MAX_KITTY_CHUNK_BYTES {
                            log::warn!(
                                "kitty 分块图像超出 {} MiB 预算,丢弃",
                                MAX_KITTY_CHUNK_BYTES >> 20
                            );
                            return;
                        }
                        // 首块:提取控制键(apc = "G<ctl>;<payload>")。
                        let ctl = self.apc[1..]
                            .split(|&b| b == b';')
                            .next()
                            .unwrap_or_default()
                            .to_vec();
                        self.kitty_partial = Some(KittyChunk { ctl, raw: payload });
                    }
                }
            }
        }
    }

    /// DCS:sixel 图像(其余 DCS 原样转发)。
    /// 判定靠解码成败:成功 = sixel,失败 = 别的 DCS(如 DECRQSS)。
    fn finish_dcs(&mut self, out: &mut Vec<StreamEvent>) {
        // 重建完整序列给 icy_sixel(参数可出现在 q 之前)。
        let mut seq = b"\x1bP".to_vec();
        seq.extend_from_slice(&self.dcs);
        seq.extend_from_slice(b"\x1b\\");
        if let Ok(img) = icy_sixel::SixelImage::decode(&seq) {
            let (w, h) = img.dimensions();
            if w > 0 && h > 0 && !img.pixels.is_empty() {
                self.flush_text(out);
                out.push(StreamEvent::Kitty(crate::kitty::KittyImage {
                    id: crate::kitty::fnv1a(&img.pixels) as u32,
                    w: w as u32,
                    h: h as u32,
                    cols: None,
                    rows: None,
                    rgba: img.pixels.clone().into(),
                    z: 0,
                    cursor_after: crate::kitty::CursorAfter::Down,
                    action: crate::kitty::KittyAction::TransmitDisplay,
                    place_after: false,
                    target: None,
                    more: None,
                }));
                return;
            }
        }
        // 非 sixel DCS:转发。
        self.text.extend_from_slice(b"\x1bP");
        self.text.extend_from_slice(&self.dcs);
        self.text.extend_from_slice(b"\x1b\\");
    }

    fn flush_text(&mut self, out: &mut Vec<StreamEvent>) {
        if self.text.is_empty() {
            return;
        }
        // 快速路径:文本无 ESC 且上一段以完整序列结尾(解析器无
        // 待决状态)时,本次不可能触发清屏 —— 跳过双解析(高吞吐
        // 输出如 cat 大文件的解析成本减半)。判定保守:误报只多跑
        // 一次检测,漏报会漏清屏,不允许。
        let needs_scan = self.clear_seq_open || self.text.contains(&0x1b);
        if needs_scan {
            // 使用内核同源解析器识别命令,支持任意分段及等价参数写法。
            let mut detector = ClearDetector::default();
            self.clear_parser.advance(&mut detector, &self.text);
            self.clear_seq_open = text_ends_in_escape_sequence(&self.text);
            if detector.cleared {
                out.push(StreamEvent::Text(std::mem::take(&mut self.text)));
                out.push(StreamEvent::ClearScreen);
                return;
            }
        }
        out.push(StreamEvent::Text(std::mem::take(&mut self.text)));
    }

    /// `st` = 原始流用 ESC \ 终止(否则 BEL)。转发必须保留原终止符,
    /// 不能硬编码成 BEL —— 有的应用(如 shell 集成脚本)两种都发。
    fn finish_osc(&mut self, out: &mut Vec<StreamEvent>, st: bool) {
        // OSC 原样转发给终端(终端处理标题/剪贴板等)。
        self.text.extend_from_slice(b"\x1b]");
        self.text.extend_from_slice(&self.osc);
        if st {
            self.text.extend_from_slice(b"\x1b\\");
        } else {
            self.text.push(0x07);
        }
        // 同时解析 133 命令块 / 1337 iTerm2 图像。
        if let Some(img) = crate::kitty::parse_iterm2(&self.osc) {
            self.flush_text(out);
            out.push(StreamEvent::Kitty(img));
        } else if let Some(m) = Self::parse_osc133(&self.osc) {
            self.flush_text(out);
            out.push(m);
        }
    }

    fn parse_osc133(buf: &[u8]) -> Option<StreamEvent> {
        let s = String::from_utf8_lossy(buf);
        let mut parts = s.split(';');
        if parts.next()? != "133" {
            return None;
        }
        match parts.next()? {
            "A" => Some(StreamEvent::PromptStart),
            // C:命令输出开始(如 "133;C" / "133;C;r")。
            "C" => Some(StreamEvent::OutputStart),
            "D" => Some(StreamEvent::Exit(parts.next()?.parse().ok().unwrap_or(0))),
            _ => None,
        }
    }
}

/// 原子发布(架构 §7):语义状态归会话线程所有,快照以 COW
/// 共享引用携带 —— UI 一次取出即得同一版本的网格与语义状态;
/// 慢 UI 下旧显示快照被覆盖,语义状态不丢、不积压事件队列。
fn publish<U: EventListener>(
    slot: &Mutex<Option<SessionUpdate>>,
    state: &SessionState<U>,
    listener: &U,
) {
    let snapshot = Arc::new(state.snapshot());
    let revision = state.meta.revision;
    {
        let mut s = slot.lock().unwrap();
        match s.as_mut() {
            Some(u) => {
                u.snapshot = snapshot;
                u.revision = revision;
            }
            None => {
                *s = Some(SessionUpdate { snapshot, revision });
            }
        }
    }
    listener.send_event(Event::Wakeup);
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_passthrough() {
        let mut sc = StreamScanner::default();
        let evs = sc.scan(b"hello");
        assert_eq!(evs.len(), 1);
        assert!(matches!(&evs[0], StreamEvent::Text(t) if t == b"hello"));
    }

    #[test]
    fn osc133_split_across_reads() {
        let mut sc = StreamScanner::default();
        assert!(sc.scan(b"\x1b]133;").is_empty());
        let evs = sc.scan(b"A\x07");
        // OSC 原样转发给终端 + 标记事件。
        assert!(matches!(&evs[0], StreamEvent::Text(_)));
        assert!(matches!(&evs[1], StreamEvent::PromptStart));
    }

    #[test]
    fn osc133_exit_code() {
        let mut sc = StreamScanner::default();
        let evs = sc.scan(b"\x1b]133;D;127\x07");
        assert!(matches!(&evs[0], StreamEvent::Text(_)));
        assert!(matches!(&evs[1], StreamEvent::Exit(127)));
    }

    #[test]
    fn kitty_image_apc() {
        let mut sc = StreamScanner::default();
        let mut seq = b"\x1b_Gf=32,s=1,v=1,i=5;".to_vec();
        seq.extend_from_slice(b"/wAA/w==");
        seq.extend_from_slice(b"\x1b\\");
        let evs = sc.scan(&seq);
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            StreamEvent::Kitty(img) => {
                assert_eq!(img.id, 5);
                assert_eq!(&*img.rgba, &[255, 0, 0, 255][..]);
            }
            other => panic!("不是 kitty 事件: {other:?}"),
        }
    }

    #[test]
    fn kitty_apc_split_across_reads() {
        let mut sc = StreamScanner::default();
        assert!(
            sc.scan(b"before\x1b_Gf=32,s=1,v=1,i=1;")
                .iter()
                .all(|e| matches!(e, StreamEvent::Text(_)))
        );
        // 载荷 = base64(0x000000ff),跨两次 scan。
        let evs = sc.scan(b"AAAA/w==\x1b\\x07fter");
        assert!(evs.iter().any(|e| matches!(e, StreamEvent::Kitty(_))));
        assert!(evs.iter().any(|e| matches!(e, StreamEvent::Text(_))));
    }

    /// P3a:kitty m=1 分块传输 —— 载荷按序拼接,控制键沿用首块,
    /// 末块(m=0)到达才产出图像。
    #[test]
    fn kitty_multipart_chunks_concatenate() {
        let mut sc = StreamScanner::default();
        // 协议:整串 base64 后切块,非末块为 4 的倍数。
        assert!(sc.scan(b"\x1b_Gf=32,s=1,v=1,i=9,m=1;QUJD\x1b\\").is_empty());
        let evs = sc.scan(b"\x1b_Gm=0;RA==\x1b\\");
        match &evs[0] {
            StreamEvent::Kitty(img) => {
                assert_eq!(img.id, 9, "控制键沿用首块");
                assert_eq!(&*img.rgba, &b"ABCD"[..], "载荷按序拼接");
            }
            other => panic!("应为图像事件: {other:?}"),
        }
    }

    /// P3a:分块中断(无 m=0 末块)永不产出半幅图像;后续流照常。
    #[test]
    fn kitty_multipart_incomplete_is_discarded() {
        let mut sc = StreamScanner::default();
        assert!(sc.scan(b"\x1b_Gf=32,s=1,v=1,m=1;QUJD\x1b\\").is_empty());
        let evs = sc.scan(b"next\x1b_Gf=32,s=1,v=1,i=5;V1hZWg==\x1b\\");
        // 新图像(m 缺省 = 无分块)应正常产出;在途半幅已被作废。
        assert!(
            evs.iter()
                .any(|e| matches!(e, StreamEvent::Kitty(i) if *i.rgba == *b"WXYZ"))
        );
        assert_eq!(
            evs.iter()
                .filter(|e| matches!(e, StreamEvent::Kitty(_)))
                .count(),
            1
        );
    }

    /// P3a:累计载荷超预算丢弃(防内存失控)。
    #[test]
    fn kitty_multipart_over_budget_is_dropped() {
        let mut sc = StreamScanner::default();
        let big = vec![b'A'; 17 * 1024 * 1024]; // base64 文本段(预算路径不解码)
        // 两个 17 MiB 块:合计 34 MiB > 32 MiB 预算。
        let mut c1 = b"\x1b_Gf=32,s=1,v=1,i=1,m=1;".to_vec();
        c1.extend_from_slice(&big);
        c1.extend_from_slice(b"\x1b\\");
        let mut c2 = b"\x1b_Gm=0;".to_vec();
        c2.extend_from_slice(&big);
        c2.extend_from_slice(b"\x1b\\");
        assert!(sc.scan(&c1).is_empty());
        let evs = sc.scan(&c2);
        assert!(!evs.iter().any(|e| matches!(e, StreamEvent::Kitty(_))));
        // 预算丢弃后扫描器可继续工作。
        assert!(
            sc.scan(b"ok")
                .iter()
                .any(|e| matches!(e, StreamEvent::Text(_)))
        );
    }

    #[test]
    fn unknown_apc_passthrough() {
        let mut sc = StreamScanner::default();
        let evs = sc.scan(b"\x1b_Xother\x1b\\");
        assert_eq!(evs.len(), 1);
        assert!(matches!(&evs[0], StreamEvent::Text(t) if t == b"\x1b_Xother\x1b\\"));
    }

    #[test]
    fn sixel_dcs_decodes() {
        // 用 icy_sixel 编码 2×2 红色方块(输出自带完整 DCS 包装)。
        let rgba = [
            255u8, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255,
        ];
        let data = icy_sixel::SixelImage::from_rgba(rgba.to_vec(), 2, 2)
            .encode()
            .expect("编码");
        let reference = icy_sixel::SixelImage::decode(data.as_bytes()).expect("参考解码");
        let mut sc = StreamScanner::default();
        let evs = sc.scan(data.as_bytes());
        assert!(
            evs.iter().any(|e| matches!(e, StreamEvent::Kitty(_))),
            "应有图像事件: {evs:?}"
        );
        for ev in evs {
            if let StreamEvent::Kitty(img) = ev {
                assert_eq!((img.w as usize, img.h as usize), reference.dimensions());
                assert_eq!(&*img.rgba, &reference.pixels[..]);
            }
        }
    }

    #[test]
    fn non_sixel_dcs_passthrough() {
        let mut sc = StreamScanner::default();
        let evs = sc.scan(b"\x1bP1$r0 q\x1b\\");
        assert!(
            evs.iter()
                .any(|e| matches!(e, StreamEvent::Text(t) if t.starts_with(b"\x1bP")))
        );
        assert!(!evs.iter().any(|e| matches!(e, StreamEvent::Kitty(_))));
    }

    /// 协议:查询必须应答(OK),放置已传输图像回 ENOENT ——
    /// 响应字节经 PTY 写回客户端程序。
    #[test]
    fn kitty_query_generates_response() {
        let mut sc = StreamScanner::default();
        let evs = sc.scan(b"\x1b_Ga=q,i=42\x1b\\");
        match &evs[0] {
            StreamEvent::KittyResponse(bytes) => {
                assert_eq!(bytes, b"\x1b_Gi=42,OK\x1b\\");
            }
            other => panic!("应为查询应答: {other:?}"),
        }
        let evs = sc.scan(b"\x1b_Ga=p,i=7\x1b\\");
        match &evs[0] {
            StreamEvent::KittyResponse(bytes) => {
                assert_eq!(bytes, b"\x1b_Gi=7,ENOENT: no such image\x1b\\");
            }
            other => panic!("应为放置应答: {other:?}"),
        }
    }

    /// P2 加固:ED2/RIS 清屏序列检测 → ClearScreen 事件
    /// (块与图像锚点失效的信号)。
    /// 传输缓存:a=T 传输并显示;之后 a=p 可从缓存重放放置。
    #[test]
    fn kitty_place_replays_cached_image() {
        let mut sc = StreamScanner::default();
        // 先传输(1 像素红色,base64)。
        let evs = sc.scan(b"\x1b_Ga=T,f=32,s=1,v=1,i=7;/wAA/w==\x1b\\");
        assert!(
            evs.iter().any(|e| matches!(e, StreamEvent::Kitty(_))),
            "a=T 显示"
        );
        // a=p 放置:命中缓存 → 放置事件 + OK 应答。
        let evs = sc.scan(b"\x1b_Ga=p,i=7\x1b\\");
        match &evs[0] {
            StreamEvent::Kitty(img) => {
                assert_eq!(img.id, 7);
                assert_eq!(&*img.rgba, &[255, 0, 0, 255][..], "像素来自缓存");
            }
            other => panic!("a=p 命中应放置图像: {other:?}"),
        }
        assert!(
            matches!(&evs[1], StreamEvent::KittyResponse(r) if r == b"\x1b_Gi=7,OK\x1b\\"),
            "命中回 OK"
        );
    }

    /// 传输缓存:a=t 仅传输不显示;随后 a=p 放置。
    #[test]
    fn kitty_transmit_only_is_not_displayed() {
        let mut sc = StreamScanner::default();
        let evs = sc.scan(b"\x1b_Ga=t,f=32,s=1,v=1,i=3;/wAA/w==\x1b\\");
        assert!(
            !evs.iter().any(|e| matches!(e, StreamEvent::Kitty(_))),
            "a=t 不产生显示事件"
        );
        let evs = sc.scan(b"\x1b_Ga=p,i=3\x1b\\");
        assert!(
            evs.iter().any(|e| matches!(e, StreamEvent::Kitty(_))),
            "a=p 从缓存放置"
        );
    }

    /// 传输缓存:a=p 未传输 → ENOENT;a=d 删除后同样 ENOENT。
    #[test]
    fn kitty_place_missing_returns_enoent() {
        let mut sc = StreamScanner::default();
        let evs = sc.scan(b"\x1b_Ga=p,i=99\x1b\\");
        assert!(
            matches!(&evs[0], StreamEvent::KittyResponse(r) if r.starts_with(b"\x1b_Gi=99,ENOENT")),
            "未传输 → ENOENT"
        );
        // 传输后删除,再放置 → ENOENT。
        sc.scan(b"\x1b_Ga=T,f=32,s=1,v=1,i=5;/wAA/w==\x1b\\");
        sc.scan(b"\x1b_Ga=d,i=5\x1b\\");
        let evs = sc.scan(b"\x1b_Ga=p,i=5\x1b\\");
        assert!(
            matches!(&evs[0], StreamEvent::KittyResponse(r) if r.starts_with(b"\x1b_Gi=5,ENOENT")),
            "删除后 → ENOENT"
        );
    }

    /// P1 回归(用户报告):历史上限 3 行时,累计滚动位移必须保持
    /// 单调 —— 历史满后锚点不能再失效(旧方案 grid+history 在
    /// history 封顶后重复编号)。
    #[test]
    fn counting_handler_tracks_scrolls_at_history_cap() {
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::term::{Config as TermConfig, Term};

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
                self.rows + 3
            }
        }
        let mut term = Term::new(
            TermConfig {
                scrolling_history: 3,
                ..Default::default()
            },
            &Dims { cols: 40, rows: 5 },
            VoidListener,
        );
        let mut parser: ansi::Processor = ansi::Processor::new();
        let mut cum: i32 = 0;
        // 影子必须跨 feed 存活(与 ptyloop 主循环一致:跨 chunk 连续)。
        let mut wrap_pending = false;
        let mut scroll_region: Option<(i32, i32, i32)> = None;
        let mut feed = |bytes: &[u8], term: &mut Term<VoidListener>, cum: &mut i32| {
            let mut h = CountingHandler {
                term,
                scrolled: 0,
                wrap_pending: &mut wrap_pending,
                scroll_region: &mut scroll_region,
            };
            parser.advance(&mut h, bytes);
            *cum += h.scrolled;
        };

        // 提示符 → 回显 → C 锚点 → 10 行输出 → 下一提示符锚点。
        feed(b"> cmd\r\n", &mut term, &mut cum);
        let c_abs = term.renderable_content().cursor.point.line.0 + cum;
        for i in 1..=10 {
            feed(format!("line{i}\r\n").as_bytes(), &mut term, &mut cum);
        }
        let p_abs = term.renderable_content().cursor.point.line.0 + cum;

        // 历史上限 3:滚动 7 次入历史(5 行窗口、回显后光标在第 1 行)。
        assert!(cum >= 7, "累计滚动位移应覆盖全部滚动,实际 {cum}");
        // 绝对行距覆盖全部 10 行输出(锚点不重复编号)。
        assert!(p_abs - c_abs >= 10, "绝对行距 = 输出行数");
        // 使用处:绝对 → grid 行;历史淘汰防御 —— 3 行历史上限,
        // 前 4 行输出已被淘汰(from_grid = -6 < -3),裁剪到仍存在的
        // 部分(line4..line10),内容正确。
        let from_grid = (c_abs - cum).max(-3);
        let to_grid = p_abs - cum - 1;
        let text = term.bounds_to_string(
            Point::new(Line(from_grid), Column(0)),
            Point::new(Line(to_grid), Column(39)),
        );
        let expect: String = (4..=10)
            .map(|i| format!("line{i}\n"))
            .collect::<String>()
            .trim_end_matches('\n')
            .to_string();
        assert_eq!(text, expect, "历史满后复制裁剪到存活部分");
    }

    #[test]
    fn dbg_wrap2() {
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::term::{Config as TermConfig, Term};
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
                self.rows + 10
            }
        }
        let mut term = Term::new(
            TermConfig {
                scrolling_history: 10,
                ..Default::default()
            },
            &Dims { cols: 10, rows: 3 },
            VoidListener,
        );
        let mut scrolled = 0i32;
        // feed1:行 0
        for (i, c) in "aaaaaaaaaaaaaaaaaaaa".chars().enumerate() {
            let before = term.renderable_content().cursor.point;
            term.input(c);
            let after = term.renderable_content().cursor.point;
            if before.line.0 == 2
                && after.line.0 == 2
                && after.column.0 <= before.column.0
                && after.column.0 <= 2
            {
                scrolled += 1;
                eprintln!("feed1[{i}] scroll before={before:?} after={after:?}");
            }
        }
        eprintln!(
            "feed1 linefeed 前: {:?}",
            term.renderable_content().cursor.point
        );
        term.linefeed();
        eprintln!(
            "feed1 linefeed 后: {:?} scrolled={scrolled}",
            term.renderable_content().cursor.point
        );
        // feed2
        for (i, c) in "bbbbbbbbbbbbbbbbbbbb".chars().enumerate() {
            let before = term.renderable_content().cursor.point;
            term.input(c);
            let after = term.renderable_content().cursor.point;
            if before.line.0 == 2
                && after.line.0 == 2
                && after.column.0 <= before.column.0
                && after.column.0 <= 2
            {
                scrolled += 1;
                eprintln!("feed2[{i}] scroll before={before:?} after={after:?}");
            }
        }
        eprintln!(
            "feed2 linefeed 前: {:?}",
            term.renderable_content().cursor.point
        );
        term.linefeed();
        eprintln!(
            "feed2 后 scrolled={scrolled} cursor={:?}",
            term.renderable_content().cursor.point
        );
        // feed3:经 parser 整块(与真实测试一致)
        use alacritty_terminal::vte::ansi;
        let mut parser: ansi::Processor = ansi::Processor::new();
        let mut wrap_pending = false;
        let mut scroll_region: Option<(i32, i32, i32)> = None;
        let mut h = crate::ptyloop::CountingHandler {
            term: &mut term,
            scrolled: 0,
            wrap_pending: &mut wrap_pending,
            scroll_region: &mut scroll_region,
        };
        parser.advance(&mut h, b"cccccccccccccccccccc\r\n");
        eprintln!(
            "feed3(parser) scrolled={} cursor={:?}",
            h.scrolled,
            term.renderable_content().cursor.point
        );
    }

    /// P1 回归:超长行 wrap 触底的滚动也要计数(打印侧滚动)。
    #[test]
    fn counting_handler_tracks_wrap_scrolls() {
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::term::{Config as TermConfig, Term};

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
                self.rows + 10
            }
        }
        let mut term = Term::new(
            TermConfig {
                scrolling_history: 10,
                ..Default::default()
            },
            &Dims { cols: 10, rows: 3 },
            VoidListener,
        );
        let mut parser: ansi::Processor = ansi::Processor::new();
        let mut cum: i32 = 0;
        // 影子必须跨 feed 存活(与 ptyloop 主循环一致:跨 chunk 连续)。
        let mut wrap_pending = false;
        let mut scroll_region: Option<(i32, i32, i32)> = None;
        let mut feed = |bytes: &[u8], term: &mut Term<VoidListener>, cum: &mut i32| {
            let mut h = CountingHandler {
                term,
                scrolled: 0,
                wrap_pending: &mut wrap_pending,
                scroll_region: &mut scroll_region,
            };
            parser.advance(&mut h, bytes);
            *cum += h.scrolled;
        };
        // 三行各 20 个字符(10 列):每行 wrap 触底 1 次(第 11 个
        // 字符)、行尾 linefeed 触底 1 次 —— feed2/feed3 各 +2。
        feed(b"aaaaaaaaaaaaaaaaaaaa\r\n", &mut term, &mut cum);
        for _ in 0..20 {
            feed(b"b", &mut term, &mut cum);
        }
        feed(b"\r\n", &mut term, &mut cum);
        for _ in 0..20 {
            feed(b"c", &mut term, &mut cum);
        }
        feed(b"\r\n", &mut term, &mut cum);
        // 精确值(pending-wrap 语义):feed1 无滚动(光标行 0→2);
        // feed2/feed3 各:第 11 字符 wrap 触底 +1、行尾 linefeed
        // 触底 +1 → 各 +2。总滚动 4。
        assert_eq!(cum, 4, "wrap 触底滚动必须精确计数");
    }

    /// P1 回归:alt-screen(vim/less)内的滚动不计入主屏累计位移 ——
    /// 退出后主屏命令块/图像锚点不得偏移。
    #[test]
    fn counting_handler_ignores_alt_screen_scrolls() {
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::term::{Config as TermConfig, Term};

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
                self.rows + 20
            }
        }
        let mut term = Term::new(
            TermConfig {
                scrolling_history: 20,
                ..Default::default()
            },
            &Dims { cols: 40, rows: 4 },
            VoidListener,
        );
        let mut parser: ansi::Processor = ansi::Processor::new();
        let mut cum: i32 = 0;
        let mut wrap_pending = false;
        let mut scroll_region: Option<(i32, i32, i32)> = None;
        let mut feed = |bytes: &[u8], term: &mut Term<VoidListener>, cum: &mut i32| {
            let mut h = CountingHandler {
                term,
                scrolled: 0,
                wrap_pending: &mut wrap_pending,
                scroll_region: &mut scroll_region,
            };
            parser.advance(&mut h, bytes);
            *cum += h.scrolled;
        };

        // 主屏:两行输出(4 行窗口,无滚动)。
        feed(b"main out 1\r\nmain out 2\r\n", &mut term, &mut cum);
        assert_eq!(cum, 0, "主屏两行不滚动");
        let anchor = term.renderable_content().cursor.point.line.0 + cum;

        // 进 alt-screen,内部大量滚动(底部换行 + 显式 scroll_up)。
        feed(b"\x1b[?1049h", &mut term, &mut cum);
        for i in 0..10 {
            feed(format!("vim line {i}\r\n").as_bytes(), &mut term, &mut cum);
        }
        feed(b"\x1b[5S", &mut term, &mut cum);
        // 退出 alt-screen。
        feed(b"\x1b[?1049l", &mut term, &mut cum);
        assert_eq!(cum, 0, "alt-screen 滚动不污染主屏累计位移");

        // 主屏继续输出 3 行(4 行窗口 → 滚动 1 行)。
        feed(b"more\r\nmore\r\nmore\r\n", &mut term, &mut cum);
        assert_eq!(cum, 2, "退出 alt 后主屏滚动正常计数(第 2、3 个换行触底)");
        let _ = anchor;
    }

    /// 测试用终端:10 列 × 5 行,历史上限 20 行。
    fn test_term() -> Term<alacritty_terminal::event::VoidListener> {
        use alacritty_terminal::term::Config as TermConfig;

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
                self.rows + 20
            }
        }
        Term::new(
            TermConfig {
                scrolling_history: 20,
                ..Default::default()
            },
            &Dims { cols: 10, rows: 5 },
            alacritty_terminal::event::VoidListener,
        )
    }

    /// 用同一 parser 与影子驱动一段字节流,返回本次计数的滚动量。
    fn feed_counts(
        parser: &mut ansi::Processor,
        term: &mut Term<alacritty_terminal::event::VoidListener>,
        wrap_pending: &mut bool,
        scroll_region: &mut Option<(i32, i32, i32)>,
        bytes: &[u8],
    ) -> i32 {
        let mut h = CountingHandler {
            term,
            scrolled: 0,
            wrap_pending,
            scroll_region,
        };
        parser.advance(&mut h, bytes);
        h.scrolled
    }

    /// 第六轮回归(用户复现):CSI ?7l(DECAWM 关闭)后,底行打印
    /// 长文本不再换行(Term::wrapline 直接返回)—— 计数必须为 0。
    #[test]
    fn counting_handler_ignores_wrap_when_line_wrap_off() {
        use alacritty_terminal::event::VoidListener;

        let mut term = test_term();
        let mut parser: ansi::Processor = ansi::Processor::new();
        let mut wrap_pending = false;
        let mut scroll_region: Option<(i32, i32, i32)> = None;
        let mut cum: i32 = 0;
        let mut feed = |bytes: &[u8], term: &mut Term<VoidListener>, cum: &mut i32| {
            *cum += feed_counts(
                &mut parser,
                term,
                &mut wrap_pending,
                &mut scroll_region,
                bytes,
            );
        };
        // 光标移到底行,关闭自动换行,打印 40 个字符(10 列)。
        feed(b"\x1b[5;1H\x1b[?7l", &mut term, &mut cum);
        feed(
            b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMN",
            &mut term,
            &mut cum,
        );
        assert_eq!(cum, 0, "DECAWM 关闭后长文本不滚动");
        assert_eq!(Dimensions::total_lines(term.grid()), 5, "历史不增长");
        // 重新开启自动换行:挂起状态仍在(与 Term 的 needs_wrap 一致),
        // 下一个字符换行触底 → +1。
        feed(b"\x1b[?7h", &mut term, &mut cum);
        feed(b"x", &mut term, &mut cum);
        assert_eq!(cum, 1, "恢复 DECAWM 后 pending wrap 触底滚动 +1");
    }

    /// 第六轮回归(用户复现):局部滚动区内的 CSI 1S 只动区域内
    /// 内容,历史不移动 —— 不能作为整个历史坐标系的平移。
    #[test]
    fn counting_handler_ignores_local_region_scrolls() {
        use alacritty_terminal::event::VoidListener;

        let mut term = test_term();
        let mut parser: ansi::Processor = ansi::Processor::new();
        let mut wrap_pending = false;
        let mut scroll_region: Option<(i32, i32, i32)> = None;
        let mut cum: i32 = 0;
        let mut feed = |bytes: &[u8], term: &mut Term<VoidListener>, cum: &mut i32| {
            *cum += feed_counts(
                &mut parser,
                term,
                &mut wrap_pending,
                &mut scroll_region,
                bytes,
            );
        };
        // 滚动区 = 第 2..4 行(0-based 1..4,区域起点不在顶部)。
        feed(b"\x1b[2;4r", &mut term, &mut cum);
        feed(b"\x1b[1S", &mut term, &mut cum);
        assert_eq!(cum, 0, "局部滚动区 CSI 1S 不计");
        assert_eq!(Dimensions::total_lines(term.grid()), 5, "历史不移动");
    }

    /// 第六轮回归(用户复现):5 行窗口 CSI 999S 被内核截断为
    /// 5 行(Term::scroll_up_relative 内 min(n, 区域高))——
    /// 计数必须按实际行数。
    #[test]
    fn counting_handler_clamps_explicit_scroll_to_screen() {
        use alacritty_terminal::event::VoidListener;

        let mut term = test_term();
        let mut parser: ansi::Processor = ansi::Processor::new();
        let mut wrap_pending = false;
        let mut scroll_region: Option<(i32, i32, i32)> = None;
        let mut cum: i32 = 0;
        let mut feed = |bytes: &[u8], term: &mut Term<VoidListener>, cum: &mut i32| {
            *cum += feed_counts(
                &mut parser,
                term,
                &mut wrap_pending,
                &mut scroll_region,
                bytes,
            );
        };
        feed(b"\x1b[999S", &mut term, &mut cum);
        assert_eq!(cum, 5, "CSI 999S 截断为屏幕高度");
    }

    /// 第六轮回归:CSI T(下移)按实际行数计数;请求 >= 屏幕高度
    /// 时整个区域被清空(不产生平移)→ 0。
    #[test]
    fn counting_handler_truncates_scroll_down() {
        use alacritty_terminal::event::VoidListener;

        let mut term = test_term();
        let mut parser: ansi::Processor = ansi::Processor::new();
        let mut wrap_pending = false;
        let mut scroll_region: Option<(i32, i32, i32)> = None;
        let mut cum: i32 = 0;
        let mut feed = |bytes: &[u8], term: &mut Term<VoidListener>, cum: &mut i32| {
            *cum += feed_counts(
                &mut parser,
                term,
                &mut wrap_pending,
                &mut scroll_region,
                bytes,
            );
        };
        feed(b"\x1b[2T", &mut term, &mut cum);
        assert_eq!(cum, -2, "CSI 2T 内容下移 2 行");
        feed(b"\x1b[999T", &mut term, &mut cum);
        assert_eq!(cum, -2, "CSI 999T >= 屏高:区域清空,无平移");
    }

    /// 第六轮回归:自定义滚动区内的 linefeed 触底是区域内部滚动
    /// (不产历史);屏幕底行在区域之外时 linefeed 不滚动 —— 都不计。
    #[test]
    fn counting_handler_ignores_local_region_linefeed() {
        use alacritty_terminal::event::VoidListener;

        let mut term = test_term();
        let mut parser: ansi::Processor = ansi::Processor::new();
        let mut wrap_pending = false;
        let mut scroll_region: Option<(i32, i32, i32)> = None;
        let mut cum: i32 = 0;
        let mut feed = |bytes: &[u8], term: &mut Term<VoidListener>, cum: &mut i32| {
            *cum += feed_counts(
                &mut parser,
                term,
                &mut wrap_pending,
                &mut scroll_region,
                bytes,
            );
        };
        feed(b"\x1b[2;4r", &mut term, &mut cum);
        feed(b"\x1b[4;1H\n", &mut term, &mut cum); // 区域底行(0-based 3)→ 区域内部滚动
        feed(b"\x1b[5;1H\n", &mut term, &mut cum); // 屏幕底行,区域之外 → 不滚动
        assert_eq!(cum, 0, "局部区域 linefeed 不计");
        assert_eq!(Dimensions::total_lines(term.grid()), 5, "历史不移动");
    }

    /// 第六轮回归:CSI M/L(行删除/插入)与显式滚动同属一类 ——
    /// 全屏区域且光标在顶部才是网格整体平移,局部区域不计。
    #[test]
    fn counting_handler_counts_delete_and_insert_lines() {
        use alacritty_terminal::event::VoidListener;

        let mut term = test_term();
        let mut parser: ansi::Processor = ansi::Processor::new();
        let mut wrap_pending = false;
        let mut scroll_region: Option<(i32, i32, i32)> = None;
        let mut cum: i32 = 0;
        let mut feed = |bytes: &[u8], term: &mut Term<VoidListener>, cum: &mut i32| {
            *cum += feed_counts(
                &mut parser,
                term,
                &mut wrap_pending,
                &mut scroll_region,
                bytes,
            );
        };
        feed(b"\x1b[1;1H\x1b[2M", &mut term, &mut cum);
        assert_eq!(cum, 2, "全屏顶部 CSI 2M 上移 2 行入历史");
        feed(b"\x1b[1;1H\x1b[2L", &mut term, &mut cum);
        assert_eq!(cum, 0, "全屏顶部 CSI 2L 下移 2 行(-2 抵消)");
        feed(b"\x1b[2;4r", &mut term, &mut cum);
        feed(b"\x1b[2;1H\x1b[2M", &mut term, &mut cum);
        assert_eq!(cum, 0, "局部区域 CSI 2M 不计");
        feed(b"\x1b[2;1H\x1b[2L", &mut term, &mut cum);
        assert_eq!(cum, 0, "局部区域 CSI 2L 不计");
    }

    /// 架构不变量(第八轮 §7):同一字节流任意切分 → 等价终端状态
    /// 与语义结果。覆盖:wrap、宽字符、DECAWM 开/关、局部滚动区、
    /// 截断滚动(CSI 999S/2T)、alt-screen 进出 —— 全部用真实
    /// alacritty 网格与回放数据,不依赖手工行号。
    #[test]
    fn chunk_split_equivalence_full_term() {
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::term::{Config as TermConfig, Term};

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
                self.rows + 200
            }
        }

        let stream: &[u8] = concat!(
            "\x1b]133;A\x07\x1b]133;C\x07> cmd\r\n",
            "out1\r\nout2\r\n",
            "\x1b]133;D;7\x07",
            "\x1b[2;4r\x1b[1S",
            "\x1b[?7l",
            "XXXXXXX",
            "\x1b[?7h",
            "汉字宽度测试",
            "\x1b[999S",
            "\x1b[2T",
            "\x1b[?1049h",
            "vim inside\r\n",
            "\x1b[?1049l",
            "tail\r\n",
        )
        .as_bytes();

        #[derive(Debug, PartialEq, Eq)]
        struct Snapshot {
            cum: i32,
            text: String,
            mode: TermMode,
            cursor: Point,
            total: usize,
        }

        fn run_chunks(chunks: &[&[u8]]) -> Snapshot {
            let mut term = Term::new(
                TermConfig {
                    scrolling_history: 200,
                    ..Default::default()
                },
                &Dims { cols: 40, rows: 6 },
                VoidListener,
            );
            let mut parser: ansi::Processor = ansi::Processor::new();
            // 影子跨 chunk 存活(与 ptyloop 主循环一致)。
            let mut wrap_pending = false;
            let mut scroll_region: Option<(i32, i32, i32)> = None;
            let mut cum: i32 = 0;
            for c in chunks {
                let mut h = CountingHandler {
                    term: &mut term,
                    scrolled: 0,
                    wrap_pending: &mut wrap_pending,
                    scroll_region: &mut scroll_region,
                };
                parser.advance(&mut h, c);
                cum += h.scrolled;
            }
            let content = term.renderable_content();
            let total = Dimensions::total_lines(term.grid());
            let screen = Dimensions::screen_lines(term.grid());
            let history = (total - screen) as i32;
            let text = term.bounds_to_string(
                Point::new(Line(-history), Column(0)),
                Point::new(Line(screen as i32 - 1), Column(39)),
            );
            Snapshot {
                cum,
                text,
                mode: *term.mode(),
                cursor: content.cursor.point,
                total,
            }
        }

        let baseline = run_chunks(&[stream]);
        for k in 1..stream.len() {
            let split = run_chunks(&[&stream[..k], &stream[k..]]);
            assert_eq!(
                split,
                baseline,
                "切分点 {k}/{}:网格内容/累计滚动/模式/光标必须等价",
                stream.len()
            );
        }
    }

    /// 阶段 3(架构 §3):会话快照只复制可见区,锚点命令在会话侧
    /// 解析。用真实 alacritty 网格 + CountingHandler 计数回放,
    /// 不依赖手工行号。
    #[test]
    fn session_snapshot_and_copy_range() {
        use crate::session::HistoryAnchor;
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::term::{Config as TermConfig, Term};

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
        let mut state = SessionState::new(Term::new(
            TermConfig {
                scrolling_history: 100,
                ..Default::default()
            },
            &Dims { cols: 40, rows: 5 },
            VoidListener,
        ));
        let mut parser: ansi::Processor = ansi::Processor::new();
        let mut wrap_pending = false;
        let mut scroll_region: Option<(i32, i32, i32)> = None;
        let mut feed = |bytes: &[u8], state: &mut SessionState<VoidListener>| {
            let mut h = CountingHandler {
                term: &mut state.term,
                scrolled: 0,
                wrap_pending: &mut wrap_pending,
                scroll_region: &mut scroll_region,
            };
            parser.advance(&mut h, bytes);
            state.meta.cum_scroll += h.scrolled;
            state.meta.revision.0 += 1;
        };

        // 提示符回显 + 10 行输出(5 行窗口 → 滚动 6 行入历史)。
        feed(b"> cmd\r\n", &mut state);
        let c_abs = state.term.renderable_content().cursor.point.line.0 + state.meta.cum_scroll;
        for i in 1..=10 {
            feed(format!("line{i}\r\n").as_bytes(), &mut state);
        }
        assert_eq!(state.meta.cum_scroll, 7, "5 行窗口 10 行输出 → 滚动 7");

        // 快照:可见区 5 行,行 0 = 命令回显(绝对行 0);不复制历史
        // (容量 = 5×40)。
        let snap = state.snapshot();
        assert_eq!(snap.dims, (40, 5));
        assert_eq!(snap.cells.len(), 5 * 40, "快照只含可见区");
        let row0: String = snap.cells[..40].iter().map(|c| c.c).collect();
        assert!(
            row0.starts_with("line7"),
            "视口顶 = 绝对行 7 = line7,实际 {row0:?}"
        );

        // 按块复制:绝对锚点范围在会话侧解析。
        let to_abs = c_abs + 7; // line8(绝对行;grid = 1)。
        let text = state
            .copy_anchors(HistoryAnchor(c_abs), 0, HistoryAnchor(to_abs), None)
            .expect("范围内复制");
        let expect: String = (1..=8)
            .map(|i| format!("line{i}\n"))
            .collect::<String>()
            .trim_end_matches('\n')
            .to_string();
        assert_eq!(text, expect, "C 行到 line8 的纯输出");

        // 锚点复制:越界(早于历史最老行)→ 显式拒绝,不 panic。
        assert_eq!(
            state.copy_anchors(HistoryAnchor(-1), 0, HistoryAnchor(11), None),
            None,
            "已淘汰锚点 → None"
        );
        // 合法锚点范围(line5 与 line6,绝对行 5..6)。
        let text = state
            .copy_anchors(HistoryAnchor(5), 0, HistoryAnchor(6), None)
            .expect("合法锚点范围");
        assert_eq!(text, "line5\nline6");

        // 跳锚点:会话侧解析(绝对行 1 = line1 行,grid -6 ≥ -7)。
        assert!(state.scroll_to_anchor(HistoryAnchor(c_abs), state.meta.invalidate_epoch));
        assert!(state.term.grid().display_offset() > 0, "视口滚回输出起点");
    }

    /// §4.4 内核交叉验证:历史上限以下,内核 total_lines 增量必须
    /// 等于影子计数的累计滚动位移(影子正确性的独立证据)。
    #[test]
    fn counting_matches_kernel_history_growth_below_cap() {
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::term::{Config as TermConfig, Term};

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
        let mut state = SessionState::new(Term::new(
            TermConfig {
                scrolling_history: 100,
                ..Default::default()
            },
            &Dims { cols: 40, rows: 5 },
            VoidListener,
        ));
        let initial_total = Dimensions::total_lines(state.term.grid());
        let mut parser: ansi::Processor = ansi::Processor::new();
        let mut wrap_pending = false;
        let mut scroll_region: Option<(i32, i32, i32)> = None;
        let mut feed = |bytes: &[u8], state: &mut SessionState<VoidListener>| {
            let mut h = CountingHandler {
                term: &mut state.term,
                scrolled: 0,
                wrap_pending: &mut wrap_pending,
                scroll_region: &mut scroll_region,
            };
            parser.advance(&mut h, bytes);
            state.meta.cum_scroll += h.scrolled;
            state.meta.revision.0 += 1;
        };
        feed(b"> cmd\r\n", &mut state);
        for i in 1..=12 {
            feed(format!("line{i}\r\n").as_bytes(), &mut state);
        }
        let now_total = Dimensions::total_lines(state.term.grid());
        assert_eq!(
            state.meta.cum_scroll as usize,
            now_total - initial_total,
            "上限以下:影子计数 == 内核历史增长"
        );
    }

    /// 已失效坐标(架构 §7):历史淘汰后的锚点复制 → 显式 None,
    /// 不 panic、不返回错位内容。
    #[test]
    fn copy_range_on_evicted_anchor_is_none() {
        use crate::session::HistoryAnchor;
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::term::{Config as TermConfig, Term};

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
                self.rows + 10
            }
        }
        let mut state = SessionState::new(Term::new(
            TermConfig {
                scrolling_history: 10,
                ..Default::default()
            },
            &Dims { cols: 40, rows: 5 },
            VoidListener,
        ));
        let mut parser: ansi::Processor = ansi::Processor::new();
        let mut wrap_pending = false;
        let mut scroll_region: Option<(i32, i32, i32)> = None;
        let mut feed = |bytes: &[u8], state: &mut SessionState<VoidListener>| {
            let mut h = CountingHandler {
                term: &mut state.term,
                scrolled: 0,
                wrap_pending: &mut wrap_pending,
                scroll_region: &mut scroll_region,
            };
            parser.advance(&mut h, bytes);
            state.meta.cum_scroll += h.scrolled;
            state.meta.revision.0 += 1;
        };
        feed(b"> cmd\r\n", &mut state);
        for i in 1..=30 {
            feed(format!("line{i}\r\n").as_bytes(), &mut state);
        }
        // 最老锚点(回显后的第一行,绝对行 1):grid = 1 - cum < -10,
        // 已被历史回收 → 显式 None。
        assert_eq!(
            state.copy_anchors(HistoryAnchor(1), 0, HistoryAnchor(2), None),
            None,
            "已淘汰锚点 → None"
        );
        // 最新锚点仍可复制。
        let last_abs = 30;
        let text = state.copy_anchors(HistoryAnchor(last_abs), 0, HistoryAnchor(last_abs), None);
        assert_eq!(text.as_deref(), Some("line30"), "存活锚点正常复制");
    }

    /// 受控写目标(PTY master 的测试替身):每次调用最多接受
    /// `window` 字节(短写),`remaining` 耗尽后返回 WouldBlock;
    /// 记录全部写入内容。
    struct MockWriter {
        buf: Vec<u8>,
        window: usize,
        /// 阻塞前还能写的字节数(耗尽后返回 WouldBlock)。
        remaining: usize,
    }

    impl Write for MockWriter {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            if self.remaining == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "buffer full",
                ));
            }
            let n = data.len().min(self.window).min(self.remaining);
            self.buf.extend_from_slice(&data[..n]);
            self.remaining -= n;
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// 统一预算(第八轮定点修复):300 KiB 输入在空队列下完整接受
    /// 并逐字节交付 —— 入队不再做第二次预算检查(旧实现 512 KiB
    /// 发送端接受后,会话端 256 KiB 预算会整块丢弃)。
    #[test]
    fn accepted_300k_input_is_delivered_fully() {
        let mut pending: VecDeque<Vec<u8>> = VecDeque::new();
        let mut queued = 0usize;
        enqueue_input(&mut pending, &mut queued, vec![7u8; 300 * 1024]);
        assert_eq!(queued, 300 * 1024, "已接受的输入不得被第二次检查丢弃");
        let mut w = MockWriter {
            buf: Vec::new(),
            window: 64 * 1024,
            remaining: usize::MAX,
        };
        let mut woff = 0;
        let inflight = std::sync::atomic::AtomicUsize::new(300 * 1024);
        let (written, done) = flush_pending(&mut w, &mut pending, &mut woff, &mut queued).unwrap();
        assert!(done, "空队列下 300 KiB 一次写完");
        assert_eq!(written, 300 * 1024);
        release_input(&inflight, written);
        assert_eq!(
            inflight.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "额度按实际写入释放"
        );
        assert_eq!(w.buf.len(), 300 * 1024);
        assert!(w.buf.iter().all(|b| *b == 7), "交付逐字节完整");
    }

    /// PTY WouldBlock 后恢复:已接受字节按序完整交付,额度按实际
    /// 写入释放,未写出的部分继续被预留(不提前释放)。
    #[test]
    fn wouldblock_recovers_and_delivers_accepted_input_in_order() {
        let mut pending: VecDeque<Vec<u8>> = VecDeque::new();
        let mut queued = 0usize;
        let chunks: Vec<Vec<u8>> = (0..3).map(|i| vec![i as u8; 64 * 1024]).collect();
        let total: usize = chunks.iter().map(Vec::len).sum();
        for c in &chunks {
            enqueue_input(&mut pending, &mut queued, c.clone());
        }
        let mut w = MockWriter {
            buf: Vec::new(),
            window: usize::MAX,
            remaining: 0, // 模拟 PTY 缓冲满。
        };
        let mut woff = 0;
        // 在途预留 = 全部已接受字节(发送端预留,出队不释放)。
        let inflight = std::sync::atomic::AtomicUsize::new(total);
        let (written, done) = flush_pending(&mut w, &mut pending, &mut woff, &mut queued).unwrap();
        assert_eq!(written, 0, "WouldBlock 一字节都写不进");
        assert!(!done);
        assert_eq!(queued, total, "已接受字节全部保留在队列里");
        // 恢复:全部按序交付,额度按实际写入释放。
        w.remaining = usize::MAX;
        let (written, done) = flush_pending(&mut w, &mut pending, &mut woff, &mut queued).unwrap();
        release_input(&inflight, written);
        assert!(done);
        assert_eq!(written, total);
        assert_eq!(queued, 0);
        assert_eq!(
            inflight.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "恢复后额度全部释放"
        );
        assert_eq!(w.buf, chunks.concat(), "恢复后按序完整交付");
    }

    /// 部分写入:written 只含实际写出字节,剩余字节继续预留;
    /// 恢复后补齐,总额度归零。
    #[test]
    fn partial_writes_release_only_written_bytes() {
        let mut pending: VecDeque<Vec<u8>> = VecDeque::new();
        let mut queued = 0usize;
        enqueue_input(&mut pending, &mut queued, vec![5u8; 100]);
        let mut w = MockWriter {
            buf: Vec::new(),
            window: usize::MAX,
            remaining: 40, // 写 40 字节后 WouldBlock。
        };
        let mut woff = 0;
        let inflight = std::sync::atomic::AtomicUsize::new(100);
        let (written, done) = flush_pending(&mut w, &mut pending, &mut woff, &mut queued).unwrap();
        assert_eq!(written, 40, "written 只含实际写出字节");
        assert!(!done);
        release_input(&inflight, written);
        assert_eq!(
            inflight.load(std::sync::atomic::Ordering::Relaxed),
            60,
            "剩余 60 字节仍被预留(出队不释放、部分写入按量释放)"
        );
        // 恢复后写完:再释放 60。
        w.remaining = usize::MAX;
        let (written, done) = flush_pending(&mut w, &mut pending, &mut woff, &mut queued).unwrap();
        assert!(done);
        assert_eq!(written, 60);
        release_input(&inflight, written);
        assert_eq!(inflight.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert_eq!(w.buf, vec![5u8; 100], "两次冲刷按序拼接完整");
    }

    /// 原子预留:多发送者并发时检查 + 预留一步完成,总预留不突破
    /// 上限(旧实现 load + fetch_add 两步之间有竞争窗口)。
    #[test]
    fn reserve_input_is_atomic_under_concurrency() {
        use std::sync::Arc;
        // 确定性部分:两个发送者各抢 MAX×2/3(无释放),只能成功一个。
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let want = MAX_INFLIGHT_INPUT_BYTES * 2 / 3;
        let c1 = counter.clone();
        let c2 = counter.clone();
        let t1 = std::thread::spawn(move || reserve_input(&c1, want));
        let t2 = std::thread::spawn(move || reserve_input(&c2, want));
        let (a, b) = (t1.join().unwrap(), t2.join().unwrap());
        assert_ne!(a, b, "2×(MAX×2/3) > MAX,恰好一个成功");
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::Relaxed),
            want,
            "成功者预留恰好一份"
        );
        // 压力部分:8 线程随机预留/释放,监视线程断言计数永不超上限。
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut workers = Vec::new();
        for seed in 0..8u64 {
            let counter = counter.clone();
            let stop = stop.clone();
            workers.push(std::thread::spawn(move || {
                let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).max(1);
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    // xorshift:每线程独立伪随机大小(≤ 64 KiB)。
                    s ^= s << 13;
                    s ^= s >> 7;
                    s ^= s << 17;
                    let len = (s % 65536 + 1) as usize;
                    if reserve_input(&counter, len) {
                        std::thread::yield_now();
                        release_input(&counter, len);
                    }
                }
            }));
        }
        let mon = counter.clone();
        let watcher = std::thread::spawn(move || {
            for _ in 0..2000 {
                let v = mon.load(std::sync::atomic::Ordering::Relaxed);
                assert!(v <= MAX_INFLIGHT_INPUT_BYTES, "并发预留突破预算: {v}");
                std::thread::sleep(std::time::Duration::from_micros(50));
            }
        });
        watcher.join().unwrap();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for t in workers {
            t.join().unwrap();
        }
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "全部释放归零"
        );
    }

    /// TermSender 端到端:300 KiB 在空队列下接受;满预算拒绝整块
    /// (不静默截断);控制命令不受预算影响;发送失败释放预留;
    /// 会话退出清零后并发释放饱和减不溢出。
    #[test]
    fn sender_accepts_300k_rejects_over_budget_and_keeps_control_free() {
        let (tx, rx) = mpsc::channel::<SessionCommand>();
        let (wake_tx, _wake_rx) = UnixStream::pair().unwrap();
        let sender = TermSender {
            tx,
            wake: Arc::new(wake_tx),
            inflight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        // 300 KiB < 512 KiB:接受(旧实现接受后可能被会话端拒绝)。
        assert!(sender.send(SessionCommand::Input(vec![0u8; 300 * 1024])));
        // 再发 300 KiB:300 + 300 > 512 → 显式拒绝整块。
        assert!(!sender.send(SessionCommand::Input(vec![0u8; 300 * 1024])));
        assert_eq!(
            sender.inflight.load(std::sync::atomic::Ordering::Relaxed),
            300 * 1024,
            "拒绝不改变在途计数"
        );
        // 控制命令不受预算影响(满预算下 Shutdown 仍可发可收)。
        assert!(sender.send(SessionCommand::Shutdown));
        assert!(matches!(rx.recv().unwrap(), SessionCommand::Input(_)));
        assert!(matches!(rx.recv().unwrap(), SessionCommand::Shutdown));
        // 会话侧:出队不释放 → 写掉后释放 → 归零。
        let inflight = &sender.inflight;
        assert_eq!(
            inflight.load(std::sync::atomic::Ordering::Relaxed),
            300 * 1024
        );
        release_input(inflight, 300 * 1024);
        assert_eq!(inflight.load(std::sync::atomic::Ordering::Relaxed), 0);
        // 接收端断开(会话结束):发送失败,新预留被释放;
        // run 循环退出路径 inflight.store(0) 后再并发释放不溢出。
        drop(rx);
        assert!(!sender.send(SessionCommand::Input(vec![0u8; 100])));
        inflight.store(0, std::sync::atomic::Ordering::Relaxed); // 模拟会话退出清零
        release_input(inflight, 300 * 1024); // 并发迟到的释放
        assert_eq!(
            inflight.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "饱和减不溢出"
        );
    }

    /// 协议应答必须送达:预算满时用户输入被拒,应答仍送达且计入
    /// 在途(会话侧写掉后释放,记账不变)。
    #[test]
    fn response_bypasses_limit_but_is_accounted() {
        let (tx, rx) = mpsc::channel::<SessionCommand>();
        let (wake_tx, _wake_rx) = UnixStream::pair().unwrap();
        let sender = TermSender {
            tx,
            wake: Arc::new(wake_tx),
            inflight: Arc::new(std::sync::atomic::AtomicUsize::new(
                MAX_INFLIGHT_INPUT_BYTES,
            )),
        };
        assert!(
            !sender.send(SessionCommand::Input(vec![0u8; 10])),
            "预算已满:用户输入被显式拒绝"
        );
        assert!(sender.send_response(vec![0x1b, b']', b'1', b'0', b';']));
        assert_eq!(
            sender.inflight.load(std::sync::atomic::Ordering::Relaxed),
            MAX_INFLIGHT_INPUT_BYTES + 5,
            "应答计入在途,写掉后释放"
        );
        assert!(matches!(rx.recv().unwrap(), SessionCommand::Input(b) if b.len() == 5));
    }

    /// 协议应答余量(纯函数):用户上限 + 余量内健康,超出告警
    /// (不拒绝 —— 应答必须送达)。
    #[test]
    fn response_slack_boundaries() {
        assert!(response_budget_ok(MAX_INFLIGHT_INPUT_BYTES));
        assert!(response_budget_ok(
            MAX_INFLIGHT_INPUT_BYTES + MAX_RESPONSE_SLACK_BYTES
        ));
        assert!(!response_budget_ok(
            MAX_INFLIGHT_INPUT_BYTES + MAX_RESPONSE_SLACK_BYTES + 1
        ));
        assert!(!response_budget_ok(usize::MAX));
    }

    /// 连续 Resize 合并与 FIFO 语义(纯逻辑,与排空循环同一实现):
    /// 连续的 configure 风暴只应用最后一个;遇其它命令先应用
    /// 合并值 —— 后续操作看到新布局与失效世代。
    #[test]
    fn resize_merger_collapses_runs_and_keeps_fifo() {
        let ws = |n: u16| alacritty_terminal::event::WindowSize {
            num_lines: n,
            num_cols: 100,
            cell_width: 8,
            cell_height: 16,
        };
        // R,R,R,Input:drain 前只有 Input 处应用 R3(前两个被合并丢弃)。
        let mut m = ResizeMerger::new();
        assert!(m.push(true, Some(ws(1))).is_none());
        assert!(m.push(true, Some(ws(2))).is_none());
        assert!(m.push(true, Some(ws(3))).is_none());
        assert_eq!(
            m.push(false, None).map(|w| w.num_lines),
            Some(3),
            "非 Resize 命令前应用最新值"
        );
        assert!(m.drain().is_none(), "已冲刷,无尾段");
        // R,Input,R:第一个 R 在 Input 前应用,第二个走尾段。
        let mut m = ResizeMerger::new();
        m.push(true, Some(ws(1)));
        assert_eq!(
            m.push(false, None).map(|w| w.num_lines),
            Some(1),
            "Input 看到 R1"
        );
        m.push(true, Some(ws(2)));
        assert_eq!(m.drain().map(|w| w.num_lines), Some(2), "尾段 R2");
        // Input,R,Shutdown:Shutdown 前应用 R(控制命令不被跨过)。
        let mut m = ResizeMerger::new();
        assert!(m.push(false, None).is_none(), "无待应用时不产出");
        m.push(true, Some(ws(9)));
        assert_eq!(
            m.push(false, None).map(|w| w.num_lines),
            Some(9),
            "Shutdown 前应用 R9"
        );
        assert!(m.drain().is_none());
    }

    /// P1 回归(brief):异步鼠标复制的稳定锚点 —— 锚点在定格后
    /// 继续滚屏,复制结果不变;若按旧网格坐标执行,此刻内容已
    /// 移位(越界检查无法识别)。
    #[test]
    fn copy_anchors_stable_across_scroll() {
        use crate::session::HistoryAnchor;
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::term::{Config as TermConfig, Term};

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
        let mut state = SessionState::new(Term::new(
            TermConfig {
                scrolling_history: 100,
                ..Default::default()
            },
            &Dims { cols: 40, rows: 5 },
            VoidListener,
        ));
        let mut parser: ansi::Processor = ansi::Processor::new();
        let mut wrap_pending = false;
        let mut scroll_region: Option<(i32, i32, i32)> = None;
        let mut feed = |bytes: &[u8], state: &mut SessionState<VoidListener>| {
            let mut h = CountingHandler {
                term: &mut state.term,
                scrolled: 0,
                wrap_pending: &mut wrap_pending,
                scroll_region: &mut scroll_region,
            };
            parser.advance(&mut h, bytes);
            state.meta.cum_scroll += h.scrolled;
            state.meta.revision.0 += 1;
        };

        // 定格锚点:line2..line4(绝对行 2..4)。
        feed(b"> cmd\r\n", &mut state);
        for i in 1..=4 {
            feed(format!("line{i}\r\n").as_bytes(), &mut state);
        }
        let (from, to) = (HistoryAnchor(2), HistoryAnchor(4));
        let before = state.copy_anchors(from, 0, to, None).expect("定格后复制");
        assert_eq!(before, "line2\nline3\nline4");

        // 继续滚屏(旧网格坐标此刻已指向别的文本)。
        for i in 5..=12 {
            feed(format!("line{i}\r\n").as_bytes(), &mut state);
        }
        let after = state
            .copy_anchors(from, 0, to, None)
            .expect("滚屏后同一锚点仍可复制");
        assert_eq!(after, before, "锚点稳定:滚屏后指向同一文本");
    }

    /// P1 回归(brief):原子批次 —— 事件与快照同槽累积,UI 一次
    /// 取出;多次发布之间事件不丢、不随快照覆盖而丢失。
    #[test]
    fn session_update_accumulates_events_atomically() {
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::term::{Config as TermConfig, Term};

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
        let mut state = SessionState::new(Term::new(
            TermConfig {
                scrolling_history: 100,
                ..Default::default()
            },
            &Dims { cols: 40, rows: 5 },
            VoidListener,
        ));
        let slot: Arc<Mutex<Option<SessionUpdate>>> = Arc::new(Mutex::new(None));

        // 批次 N:两个语义事件 + 发布。UI 取出 → 完整 N。
        state.apply_event(BlockEvent::PromptStart { line: 1, col: 0 });
        state.apply_event(BlockEvent::OutputStart(1));
        state.meta.revision.0 += 1;
        publish(&slot, &state, &VoidListener);
        let n = slot.lock().unwrap().take().expect("批次 N");
        assert_eq!(n.revision.0, 1);
        assert_eq!(n.snapshot.blocks.blocks().len(), 1, "N 含块");
        assert_eq!(n.snapshot.blocks.blocks()[0].prompt.0, 1);

        // 批次 N+1 正在处理(事件已应用、尚未发布):槽为空,
        // UI 只能拿到完整 N(已在上一步取走),拿不到混合状态。
        state.apply_event(BlockEvent::Exit { code: 7 });
        assert!(slot.lock().unwrap().is_none(), "未发布的中间态不可见");
        // 槽仍空时 UI 持有的 N 快照不受影响(COW:旧版本不变)。
        assert_eq!(
            n.snapshot.blocks.blocks().len(),
            1,
            "旧快照不被会话变更污染"
        );

        // 发布 N+1 → UI 取到完整 N+1(块含退出码)。
        state.meta.revision.0 += 1;
        publish(&slot, &state, &VoidListener);
        let n1 = slot.lock().unwrap().take().expect("批次 N+1");
        assert_eq!(n1.revision.0, 2);
        assert_eq!(n1.snapshot.blocks.blocks().len(), 1);
        assert_eq!(n1.snapshot.blocks.blocks()[0].exit, Some(7), "N+1 语义完整");

        // 慢消费者:连续 5 个批次不取出 —— 槽只有最新一份,
        // 语义状态在会话侧完整累积(不丢、队列有界)。
        for i in 2..=6 {
            state.apply_event(BlockEvent::PromptStart {
                line: i * 10,
                col: 0,
            });
            state.meta.revision.0 += 1;
            publish(&slot, &state, &VoidListener);
        }
        let latest = slot.lock().unwrap().take().expect("最新批次");
        assert_eq!(latest.revision.0, 7, "1(N) + 1(N+1) + 5 个批次");
        assert_eq!(
            latest.snapshot.blocks.blocks().len(),
            6,
            "慢 UI 下语义状态全部保留"
        );
        assert!(slot.lock().unwrap().is_none());
    }

    /// 发送端输入预算(架构 §6):输入在进入跨线程队列**之前**受控;
    /// 拒绝整块不静默截断;控制命令(Shutdown)不受预算影响;
    /// 会话出队后预算释放。
    #[test]
    fn sender_input_budget_refuses_over_limit() {
        let (tx, rx) = mpsc::channel::<SessionCommand>();
        let (w_tx, _w_rx) = UnixStream::pair().unwrap();
        let sender = TermSender {
            tx,
            wake: Arc::new(w_tx),
            inflight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        // 4 × 128 KiB 恰好占满 512 KiB。
        for v in 1..=4u8 {
            assert!(sender.send(SessionCommand::Input(vec![v; 128 * 1024])));
        }
        assert_eq!(
            sender.inflight.load(std::sync::atomic::Ordering::Relaxed),
            512 * 1024
        );
        // 再加 1 字节 → 拒绝;在途计数不变。
        assert!(!sender.send(SessionCommand::Input(vec![9u8; 1])));
        assert_eq!(
            sender.inflight.load(std::sync::atomic::Ordering::Relaxed),
            512 * 1024
        );
        // 会话出队后预算释放(与 run 循环同一减计数语义);
        // 队列此刻只有 4 块输入,while-let 消费安全。
        while let Ok(SessionCommand::Input(bytes)) = rx.try_recv() {
            sender
                .inflight
                .fetch_sub(bytes.len(), std::sync::atomic::Ordering::Relaxed);
        }
        assert_eq!(
            sender.inflight.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "出队后归零"
        );
        // 控制命令不受预算影响(满预算时 Shutdown 照样可发可收)。
        for v in 1..=4u8 {
            assert!(sender.send(SessionCommand::Input(vec![v; 128 * 1024])));
        }
        assert!(sender.send(SessionCommand::Shutdown));
        // 排空(与 run 循环同一减计数语义);Shutdown 排在输入之后、
        // 不受预算阻塞。
        let mut got_shutdown = false;
        loop {
            match rx.try_recv() {
                Ok(SessionCommand::Input(bytes)) => {
                    sender
                        .inflight
                        .fetch_sub(bytes.len(), std::sync::atomic::Ordering::Relaxed);
                }
                Ok(SessionCommand::Shutdown) => got_shutdown = true,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        assert!(got_shutdown, "控制命令不被输入预算阻塞");
        assert_eq!(
            sender.inflight.load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        // 大段粘贴不截半:一整块要么全收要么全拒(顺序由 FIFO 保证)。
        assert!(sender.send(SessionCommand::Input(vec![7u8; 256 * 1024])));
        assert!(sender.send(SessionCommand::Input(vec![8u8; 256 * 1024])));
        assert!(!sender.send(SessionCommand::Input(vec![6u8; 1])));
        let first = match rx.try_recv() {
            Ok(SessionCommand::Input(b)) => b,
            other => panic!("期望整块输入: {other:?}"),
        };
        assert_eq!(first.len(), 256 * 1024);
        assert!(first.iter().all(|&b| b == 7), "整块不被截断/乱序");
    }

    /// 六/二:resize 与输出交错 —— 仅实际尺寸变化才失效语义
    /// 状态并推进失效代数(与快照同批);同尺寸 resize 保留块。
    #[test]
    fn resize_invalidates_semantics_only_on_dim_change() {
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::term::{Config as TermConfig, Term};

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
        let mut state = SessionState::new(Term::new(
            TermConfig {
                scrolling_history: 100,
                ..Default::default()
            },
            &Dims { cols: 40, rows: 5 },
            VoidListener,
        ));
        // 一个块。
        state.apply_event(BlockEvent::PromptStart { line: 1, col: 0 });
        state.apply_event(BlockEvent::Exit { code: 7 });
        assert_eq!(state.blocks.blocks().len(), 1);

        // 同尺寸 resize(surface 重建场景):块保留、代数不动。
        let changed = state.resize(40, 5);
        assert!(!changed);
        assert_eq!(state.blocks.blocks().len(), 1, "同尺寸不得清空语义状态");
        assert_eq!(state.meta.invalidate_epoch, crate::session::AnchorEpoch(0));

        // 尺寸变化:块/图像失效、代数 +1;下一条 prompt 重建。
        let changed = state.resize(30, 6);
        assert!(changed);
        assert_eq!(state.blocks.blocks().len(), 0, "reflow 使锚点失效");
        assert_eq!(state.meta.invalidate_epoch, crate::session::AnchorEpoch(1));
        state.apply_event(BlockEvent::PromptStart { line: 0, col: 0 });
        assert_eq!(state.blocks.blocks().len(), 1, "失效后下一条 prompt 重建");
    }

    #[test]
    fn clear_detection_survives_every_chunk_boundary() {
        for seq in [b"\x1b[2J".as_slice(), b"\x1b[3J", b"\x1bc", b"\x1b[02J"] {
            for split in 0..=seq.len() {
                let mut scanner = StreamScanner::default();
                let mut events = scanner.scan(&seq[..split]);
                events.extend(scanner.scan(&seq[split..]));
                assert_eq!(
                    events
                        .iter()
                        .filter(|e| matches!(e, StreamEvent::ClearScreen))
                        .count(),
                    1,
                    "sequence {seq:?}, split {split}"
                );
                let bytes: Vec<u8> = events
                    .into_iter()
                    .filter_map(|e| match e {
                        StreamEvent::Text(bytes) => Some(bytes),
                        _ => None,
                    })
                    .flatten()
                    .collect();
                assert_eq!(bytes, seq);
                assert!(
                    !scanner
                        .scan(b"plain")
                        .iter()
                        .any(|e| matches!(e, StreamEvent::ClearScreen))
                );
            }
        }
    }

    #[test]
    fn clear_detection_ignores_non_clear_sequences() {
        for seq in [b"\x1b]2;title[2J\x07".as_slice(), b"\x1b[?2J", b"\x1b[2K"] {
            for split in 0..=seq.len() {
                let mut scanner = StreamScanner::default();
                let mut events = scanner.scan(&seq[..split]);
                events.extend(scanner.scan(&seq[split..]));
                assert!(
                    !events.iter().any(|e| matches!(e, StreamEvent::ClearScreen)),
                    "control string {seq:?}, split {split}"
                );
            }
        }
    }

    #[test]
    fn clear_screen_sequences_emit_event() {
        let mut sc = StreamScanner::default();
        // ED2:清屏。
        let evs = sc.scan(b"a\x1b[2Jb");
        assert!(
            evs.iter().any(|e| matches!(e, StreamEvent::Text(_))),
            "文本照常转发"
        );
        assert!(
            evs.iter().any(|e| matches!(e, StreamEvent::ClearScreen)),
            "\x1b[2J 应触发失效事件"
        );
        // ED3:清滚动历史。
        let evs = sc.scan(b"\x1b[3J");
        assert!(evs.iter().any(|e| matches!(e, StreamEvent::ClearScreen)));
        // RIS:硬重置。
        let evs = sc.scan(b"\x1bc");
        assert!(evs.iter().any(|e| matches!(e, StreamEvent::ClearScreen)));
        // 普通文本不触发。
        let evs = sc.scan(b"hello");
        assert!(!evs.iter().any(|e| matches!(e, StreamEvent::ClearScreen)));
    }

    /// 快速路径判定:文本以未完成转义序列结尾 → 下次必须跑检测。
    #[test]
    fn escape_sequence_tail_detection() {
        assert!(!text_ends_in_escape_sequence(b""));
        assert!(!text_ends_in_escape_sequence(b"plain"));
        assert!(text_ends_in_escape_sequence(b"abc\x1b"), "孤立 ESC");
        assert!(text_ends_in_escape_sequence(b"\x1b[2"), "CSI 无终止字节");
        assert!(
            text_ends_in_escape_sequence(b"\x1b[?1049"),
            "参数后无终止字节"
        );
        assert!(
            text_ends_in_escape_sequence(b"\x1b[2J\x1b"),
            "末尾孤立 ESC 覆盖完整序列"
        );
        assert!(!text_ends_in_escape_sequence(b"\x1b[2J"));
        assert!(!text_ends_in_escape_sequence(b"\x1b[K"));
        assert!(!text_ends_in_escape_sequence(b"\x1b[?1049h"));
        assert!(!text_ends_in_escape_sequence(b"done\x1b[2J\x1b[3J"));
        assert!(!text_ends_in_escape_sequence(b"\x1bc"), "RIS 完整");
    }

    /// 快速路径不破坏跨 flush 分段检测:前段以未完成 CSI 结尾、
    /// 后段无 ESC —— `clear_seq_open` 使检测仍然运行(修复前
    /// 双解析优化若漏此路径会漏清屏)。
    #[test]
    fn split_clear_across_esc_free_tail_is_detected() {
        let mut sc = StreamScanner::default();
        // "\x1b[2" + flush 边界 + "J"(后段无 ESC)。
        let evs = sc.scan(b"a\x1b[2");
        assert!(
            evs.iter()
                .any(|e| matches!(e, StreamEvent::Text(t) if t.ends_with(b"\x1b[2"))),
            "前段文本原样转发"
        );
        assert!(
            evs.iter().all(|e| !matches!(e, StreamEvent::ClearScreen)),
            "前段未完成,不触发"
        );
        let evs = sc.scan(b"Jb");
        assert!(
            evs.iter().any(|e| matches!(e, StreamEvent::ClearScreen)),
            "后段无 ESC 也须检测出清屏(clear_seq_open 路径)"
        );
        assert!(evs.iter().any(|e| matches!(e, StreamEvent::Text(_))));
        // 之后纯文本不再触发(快速路径无残留状态)。
        let evs = sc.scan(b"plain");
        assert!(!evs.iter().any(|e| matches!(e, StreamEvent::ClearScreen)));
    }

    #[test]
    fn st_esc_inside_apc() {
        // APC 内容中的孤立 ESC 并入内容;畸形 kitty 序列按协议丢弃。
        let mut sc = StreamScanner::default();
        assert!(sc.scan(b"\x1b_Gx\x1bz\x1b\\").is_empty());
    }

    /// 写队列:P1b 回归 —— 非阻塞写端缓冲满时输入不能丢,
    /// 必须留在队列里,空间释放后按序全部到达。
    #[test]
    fn write_queue_retains_and_flushes_when_blocked() {
        use std::os::fd::FromRawFd;

        // 管道 = PTY master 的测试替身(同为 O_NONBLOCK + 满则 WouldBlock)。
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let mut r = unsafe { std::fs::File::from_raw_fd(fds[0]) };
        let mut w = unsafe { std::fs::File::from_raw_fd(fds[1]) };
        let fl = unsafe { libc::fcntl(fds[1], libc::F_GETFL) };
        assert!(fl >= 0);
        assert_eq!(
            unsafe { libc::fcntl(fds[1], libc::F_SETFL, fl | libc::O_NONBLOCK) },
            0
        );

        // 填满管道(4096 ≤ PIPE_BUF,每次写要么全进要么 WouldBlock)。
        let fill = [0xabu8; 4096];
        loop {
            match w.write(&fill) {
                Ok(n) => assert!(n > 0),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("填充失败: {e}"),
            }
        }

        // 入队 12 KiB:缓冲满时一字节都进不去,数据必须全部保留。
        let chunks: Vec<Vec<u8>> = vec![vec![1u8; 4096], vec![2u8; 4096], vec![3u8; 4096]];
        let total: usize = chunks.iter().map(Vec::len).sum();
        let mut pending: VecDeque<Vec<u8>> = chunks.iter().cloned().collect();
        let mut woff = 0;
        let mut queued_bytes = total;
        let (written, done) =
            flush_pending(&mut w, &mut pending, &mut woff, &mut queued_bytes).unwrap();
        assert_eq!(written, 0, "缓冲满时一字节都写不进");
        assert!(!done);
        assert_eq!(pending.len(), 3, "缓冲满时入队数据不得丢失");
        assert_eq!(queued_bytes, total, "未写出的字节仍计在队列预算里");

        // 边释放空间边冲刷,直到写完;期间不 panic、不丢字节。
        let mut sink = [0u8; 4096];
        while !flush_pending(&mut w, &mut pending, &mut woff, &mut queued_bytes)
            .unwrap()
            .1
        {
            let n = r.read(&mut sink).unwrap();
            assert!(n > 0, "flush 返回未完成时管道必有数据可读");
        }
        assert!(pending.is_empty());
        assert_eq!(woff, 0);
        assert_eq!(queued_bytes, 0, "写空后预算计数归零");

        // 全部到达且按序:关闭写端,读净管道,末尾必须恰好是三个 chunk 的拼接。
        drop(w);
        let mut all = Vec::new();
        r.read_to_end(&mut all).unwrap();
        assert!(all.len() >= total);
        let expect: Vec<u8> = chunks.concat();
        assert_eq!(
            &all[all.len() - total..],
            &expect[..],
            "写队列必须按序完整交付"
        );
    }

    /// 事件流归一化:连续 Text 折叠成一条(scan 每次调用都会 flush_text,
    /// 分片方式不影响语义 —— 消费者按字节流处理文本),非文本事件保持
    /// 原序。StreamEvent 未派生 PartialEq,用 Debug 字符串比较。
    fn events_norm(evs: &[StreamEvent]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut buf: Vec<u8> = Vec::new();
        for e in evs {
            match e {
                StreamEvent::Text(t) => buf.extend_from_slice(t),
                other => {
                    if !buf.is_empty() {
                        out.push(format!("T({buf:?})"));
                        buf.clear();
                    }
                    out.push(format!("{other:?}"));
                }
            }
        }
        if !buf.is_empty() {
            out.push(format!("T({buf:?})"));
        }
        out
    }

    /// P1c 回归:同一输入按任意位置切分扫描,事件流必须逐字节等价。
    /// 覆盖:chunk 边界处的孤立 ESC、OSC ST 跨 chunk、APC/DCS 跨 chunk。
    #[test]
    fn chunk_boundaries_do_not_change_events() {
        let rgba = [
            255u8, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255,
        ];
        let sixel = icy_sixel::SixelImage::from_rgba(rgba.to_vec(), 2, 2)
            .encode()
            .expect("编码")
            .into_bytes();
        let mut kitty = b"\x1b_Gf=32,s=1,v=1,i=7;".to_vec();
        kitty.extend_from_slice(b"/wAA/w==");
        kitty.extend_from_slice(b"\x1b\\");
        // P3a 标准样本:2×2 红色块按 kitty m=1 分 3 块传输。
        // 整串 base64 后切 3 段(非末块为 4 的倍数)。
        let mut multi = b"\x1b_Ga=T,f=32,s=2,v=2,i=8,m=1;".to_vec();
        multi.extend_from_slice(b"/wAA//8A");
        multi.extend_from_slice(b"\x1b\\");
        multi.extend_from_slice(b"\x1b_Gm=1;");
        multi.extend_from_slice(b"AP//AAD/");
        multi.extend_from_slice(b"\x1b\\");
        multi.extend_from_slice(b"\x1b_Gm=0;");
        multi.extend_from_slice(b"/wAA/w==");
        multi.extend_from_slice(b"\x1b\\");

        let cases: Vec<Vec<u8>> = vec![
            b"plain text".to_vec(),
            b"a\x1bb".to_vec(),             // ESC 后接普通字符(不是序列)
            b"\x1b]133;A\x07".to_vec(),     // OSC 133 BEL 终止
            b"\x1b]133;D;1\x1b\\".to_vec(), // OSC 133 ST 终止
            b"\x1b]0;title\x1b\\".to_vec(), // 普通 OSC:转发须保留 ST
            b"\x1b]2;bell\x07".to_vec(),    // 普通 OSC:转发须保留 BEL
            kitty,                          // APC 图像
            multi,                          // kitty m=1 分块(3 块)
            sixel,                          // DCS sixel 图像
            b"\x1bP1$r0 q\x1b\\".to_vec(),  // 非 sixel DCS 转发
        ];
        for data in cases {
            let full = {
                let mut sc = StreamScanner::default();
                events_norm(&sc.scan(&data))
            };
            for k in 0..=data.len() {
                let mut sc = StreamScanner::default();
                let mut raw = sc.scan(&data[..k]);
                raw.extend(sc.scan(&data[k..]));
                let evs = events_norm(&raw);
                assert_eq!(evs, full, "切分点 {k}/{}, 输入 {data:?}", data.len());
            }
        }
    }

    /// 终止符回放:ST 终止的 OSC 转发后仍是 ST,不能改写为 BEL。
    #[test]
    fn osc_forward_keeps_st_terminator() {
        let mut sc = StreamScanner::default();
        let evs = sc.scan(b"\x1b]0;tay\x1b\\");
        let text = match &evs[0] {
            StreamEvent::Text(t) => t,
            other => panic!("应是文本: {other:?}"),
        };
        assert_eq!(text, b"\x1b]0;tay\x1b\\");
    }
}
