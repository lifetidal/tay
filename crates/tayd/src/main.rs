//! tayd — Tay 常驻进程(M1:daemon + layer-shell 呼出)。
//!
//! 环境变量:
//! - `TAY_SHOT=路径[,帧数]` — 渲染 N 帧后抓取当前帧存为 PNG 并退出(无头验证用)
//! - `TAY_BENCH=帧数` — 启动后灌入 3000 行输出,统计帧时间后退出
//! - `TAY_EXEC=命令` — 启动后自动执行(端到端验证输入→PTY→输出→渲染链路)
//! - `TAY_START_VISIBLE=1` — 启动即呼出(覆盖配置 start_hidden,截图验证用)

mod app;
mod hotkey;
mod ipc;
mod keys;
mod ptyloop;

use alacritty_terminal::tty;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // TERM=xterm-256color(alacritty terminfo 缺失时)/ COLORTERM=truecolor。
    tty::setup_env();

    let mut config = tay_config::Config::load();
    if std::env::var("TAY_START_VISIBLE").is_ok_and(|v| v == "1") {
        config.start_hidden = false;
    }
    if let Ok(shell) = std::env::var("TAY_SHELL") {
        config.shell = shell;
    }
    log::info!(
        "tayd 启动:shell={} 字体={} {}pt 隐藏启动={}",
        config.shell_command(),
        config.font.family,
        config.font.size,
        config.start_hidden
    );

    let shot = std::env::var("TAY_SHOT").ok().map(|v| {
        let (path, frames) =
            v.split_once(',').map_or((v.as_str(), 90), |(p, f)| (p, f.parse().unwrap_or(90)));
        (path.to_string(), frames)
    });
    let bench_frames = std::env::var("TAY_BENCH").ok().and_then(|v| v.parse::<u32>().ok());
    let exec = std::env::var("TAY_EXEC").ok();

    // 跨线程唤醒通道(PTY/IPC/热键 → 事件循环)。
    let (wake_tx, wake_rx) = std::sync::mpsc::channel();

    // IPC socket 绑定(主线程;accept 线程延迟到 PTY fork 之后)。
    let ipc_listener = match ipc::bind() {
        Ok(l) => Some(l),
        Err(e) => {
            log::warn!("IPC 不可用(可能已有实例): {e}");
            std::process::exit(1);
        },
    };
    let (ipc_tx, ipc_rx) = ipc::channel();

    // 全局热键通道(portal 线程延迟到 PTY fork 之后启动)。
    let (hotkey_tx, hotkey_rx) = std::sync::mpsc::channel();

    let app = app::TayApp::new(
        config,
        shot,
        bench_frames,
        exec,
        ipc_listener,
        ipc_tx,
        ipc_rx,
        hotkey_tx,
        hotkey_rx,
        wake_tx.clone(),
    );
    if let Err(e) = tay_window::run("tay", app, (wake_tx, wake_rx)) {
        log::error!("事件循环退出: {e}");
    }
}
