#![no_main]

//! 任意字节流 → 终端协议解析管线。
//!
//! 复用的入口与组合与 ptyloop.rs 的
//! `chunk_split_equivalence_full_term` / `chunk_boundaries_do_not_change_events`
//! 测试一致:
//!
//! 1. `StreamScanner::scan`(生产 run() 中的同一实例,跨 chunk 存活):
//!    ANSI/CSI 分帧、OSC 133、APC kitty graphics、DCS sixel 解码;
//! 2. Text 事件喂给 `alacritty_terminal::vte::ansi::Processor` +
//!    `CountingHandler` 代理的真实 `Term` 网格(与生产路径相同)。
//!
//! 为什么在 target 内 include! 源码:StreamScanner / StreamEvent /
//! CountingHandler 都是 tayd crate 的私有项,外部 crate 无法直接调用;
//! 约束又不允许修改 crates/ 下源码。build.rs 把 ptyloop.rs 顶部的
//! `//!` 文档注释剥离后复制到 OUT_DIR(其余逐字保留),本 target 在
//! 同一模块作用域内 include!,从而直接编译、执行**同一份**管线代码。
//!
//! 不包 catch_unwind:管线在单元测试中就是裸调,panic 即真实缺陷,
//! 让 libFuzzer 把 panic 暴露为崩溃。

use libfuzzer_sys::fuzz_target;

#[path = "../../crates/tayd/src/kitty.rs"]
mod kitty;
#[path = "../../crates/tayd/src/blocks.rs"]
mod blocks;
#[path = "../../crates/tayd/src/terminal.rs"]
mod terminal;
#[path = "../../crates/tayd/src/session.rs"]
mod session;

mod ptyloop {
    include!(concat!(env!("OUT_DIR"), "/ptyloop_fuzz.rs"));

    /// fuzz 入口:与生产 `run()` 相同的扫描器/解析器/Term 组合。
    /// `data` 按 16 KiB 分块(生产 PTY 读缓冲大小),扫描器状态跨块存活。
    pub fn fuzz_stream(data: &[u8]) {
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::term::Config as TermConfig;

        // 与 chunk_split_equivalence_full_term 相同的真实网格
        // (40 列 × 6 行,历史 200 行;小网格更容易触发换行/滚动边界)。
        struct Dims;
        impl Dimensions for Dims {
            fn columns(&self) -> usize {
                40
            }
            fn screen_lines(&self) -> usize {
                6
            }
            fn total_lines(&self) -> usize {
                206
            }
        }

        let mut term: Term<VoidListener> = Term::new(
            TermConfig {
                scrolling_history: 200,
                ..Default::default()
            },
            &Dims,
            VoidListener,
        );
        let mut parser: ansi::Processor = ansi::Processor::new();
        // 影子状态跨 chunk 存活(与 ptyloop 主循环一致)。
        let mut wrap_pending = false;
        let mut scroll_region: Option<(i32, i32, i32)> = None;
        let mut scanner = StreamScanner::default();

        // 在中点强制切一刀:即使输入很小也覆盖「孤立 ESC 挂起、
        // OSC/APC/DCS 跨 chunk」等跨读状态(见
        // chunk_boundaries_do_not_change_events 测试);其余按生产
        // 16 KiB PTY 读缓冲继续切分。扫描器状态全程存活。
        let chunks: Vec<&[u8]> = if data.len() >= 2 {
            let mid = data.len() / 2;
            data[..mid]
                .chunks(16 * 1024)
                .chain(data[mid..].chunks(16 * 1024))
                .collect()
        } else {
            data.chunks(16 * 1024).collect()
        };

        for chunk in chunks {
            for ev in scanner.scan(chunk) {
                if let StreamEvent::Text(bytes) = ev {
                    let mut h = CountingHandler {
                        term: &mut term,
                        palette: None,
                        cell: (8, 16),
                        responses: Vec::new(),
                        response_listener: None,
                        scrolled: 0,
                        wrap_pending: &mut wrap_pending,
                        scroll_region: &mut scroll_region,
                    };
                    parser.advance(&mut h, &bytes);
                    let _ = h.scrolled;
                }
                // Kitty 图像已在 scan() 内解码(kitty base64/PNG);
                // OSC 133 / ClearScreen / 应答事件无额外解析步骤。
            }
        }

        // 读取最终网格状态,保证整条管线结果被使用。
        let _ = term.renderable_content();
    }
}

/// 与生产 PTY 读缓冲(16 KiB)对齐的单次输入上限:64 KiB。
const MAX_INPUT: usize = 64 * 1024;

fuzz_target!(|data: &[u8]| {
    let data = &data[..data.len().min(MAX_INPUT)];
    ptyloop::fuzz_stream(data);
});
