//! tayd — Tay 常驻进程(M0:单进程版,daemon 化在 M1)。
//!
//! 环境变量:
//! - `TAY_SHOT=路径[,帧数]` — 渲染 N 帧后抓取当前帧存为 PNG 并退出(无头验证用)
//! - `TAY_BENCH=帧数` — 启动后灌入 3000 行输出,统计帧时间后退出
//! - `TAY_EXEC=命令` — 启动后自动执行(端到端验证输入→PTY→输出→渲染链路)

mod app;
mod keys;

use alacritty_terminal::tty;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // TERM=xterm-256color(alacritty terminfo 缺失时)/ COLORTERM=truecolor。
    tty::setup_env();

    let config = tay_config::Config::load();
    log::info!(
        "tayd 启动:shell={} 字体={} {}pt",
        config.shell_command(),
        config.font.family,
        config.font.size
    );

    let shot = std::env::var("TAY_SHOT").ok().map(|v| {
        let (path, frames) = v.split_once(',').map_or((v.as_str(), 90), |(p, f)| {
            (p, f.parse().unwrap_or(90))
        });
        (path.to_string(), frames)
    });

    let bench_frames = std::env::var("TAY_BENCH").ok().and_then(|v| v.parse::<u32>().ok());
    let exec = std::env::var("TAY_EXEC").ok();

    let app = app::TayApp::new(config, shot, bench_frames, exec);
    if let Err(e) = tay_window::run("tay", (1100, 700), app) {
        log::error!("事件循环退出: {e}");
    }
}
