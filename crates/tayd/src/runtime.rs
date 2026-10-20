use crate::{app, ipc};
use alacritty_terminal::tty;

use std::io::Write;

pub(crate) fn run() {
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
        let (path, frames) = v
            .split_once(',')
            .map_or((v.as_str(), 90), |(p, f)| (p, f.parse().unwrap_or(90)));
        (path.to_string(), frames)
    });
    let bench_frames = std::env::var("TAY_BENCH")
        .ok()
        .and_then(|v| v.parse::<u32>().ok());
    let exec = std::env::var("TAY_EXEC").ok();
    let quickselect = std::env::var("TAY_QUICKSELECT").is_ok_and(|v| v == "1");
    let vi_at_start = std::env::var("TAY_VI").is_ok_and(|v| v == "1");
    let fold_at_start = std::env::var("TAY_FOLD").is_ok_and(|v| v == "1");
    // 验收钩子:第 11 帧注入 IME Commit(中文输入链路验证)、
    // 第 15 帧强制 surface 重建(输出回退路径验证,单显示器机器
    // 无法拔线的替代手段)。
    let ime_at_start = std::env::var("TAY_IME").ok();
    let rebind_at_start = std::env::var("TAY_REBIND").is_ok_and(|v| v == "1");
    // 验收钩子:第 16 帧注入滚轮滚动、第 17 帧合成拖选并复制
    // (滚轮/选择复制链路实机验证)、第 18/19 帧注入两次 300 KiB
    // 大段粘贴(统一输入预算端到端 + 拒绝反馈实机验证;配慢消费
    // shell 脚本确定性触发第二次拒绝)。
    let scroll_at_start = std::env::var("TAY_SCROLL").is_ok_and(|v| v == "1");
    let sel_at_start = std::env::var("TAY_SEL").is_ok_and(|v| v == "1");
    let paste_at_start = std::env::var("TAY_PASTE").is_ok_and(|v| v == "1");
    let slowui_at_start = std::env::var("TAY_SLOWUI").is_ok_and(|v| v == "1");

    // 跨线程唤醒管道(PTY/IPC/热键 → 事件循环;管道可进 poll,
    // 事件循环空闲时才能无限阻塞而不是轮询)。
    let (wake_tx, wake_rx) = std::os::unix::net::UnixStream::pair().expect("创建唤醒管道失败");
    wake_tx.set_nonblocking(true).ok();
    wake_rx.set_nonblocking(true).ok();
    let wake_tx = std::sync::Arc::new(wake_tx);

    // 单实例互斥(flock);并发冷启动的失败方静默退出。
    let _lock = match ipc::acquire_lock() {
        Ok(l) => l,
        Err(e) => {
            log::info!("{e}(本进程退出)");
            std::process::exit(0);
        }
    };

    // 主线程绑定 IPC;accept 服务在初始会话就绪后启动。
    let ipc_listener = match ipc::bind() {
        Ok(l) => Some(l),
        Err(e) => {
            log::error!("IPC 不可用: {e}");
            std::process::exit(1);
        }
    };
    let (ipc_tx, ipc_rx) = ipc::channel();

    // 全局热键通道(portal 服务在初始会话就绪后启动)。
    let (hotkey_tx, hotkey_rx) = std::sync::mpsc::sync_channel(64);

    // 配置热重载:监视 config.lua(变化 → 唤醒 UI 线程重载)。
    let config_path = tay_config::Config::config_path();
    if let Some(parent) = config_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let wake_cfg = wake_tx.clone();
    std::thread::Builder::new()
        .name("cfg-watch".into())
        .spawn(move || {
            use notify::{RecommendedWatcher, RecursiveMode, Watcher};
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            let mut watcher = match RecommendedWatcher::new(
                move |event: notify::Result<notify::Event>| {
                    let _ = tx.try_send(event);
                },
                notify::Config::default(),
            ) {
                Ok(w) => w,
                Err(e) => {
                    log::warn!("配置监视不可用: {e}");
                    return;
                }
            };
            if let Some(dir) = config_path.parent()
                && watcher.watch(dir, RecursiveMode::NonRecursive).is_err()
            {
                return;
            }
            while rx.recv().is_ok() {
                let _ = (&*wake_cfg).write(&[0]);
            }
        })
        .ok();

    let output = if config.output.is_empty() {
        None
    } else {
        Some(config.output.clone())
    };
    let app = app::TayApp::new(
        config,
        shot,
        bench_frames,
        exec,
        quickselect,
        vi_at_start,
        fold_at_start,
        ime_at_start,
        rebind_at_start,
        scroll_at_start,
        sel_at_start,
        paste_at_start,
        slowui_at_start,
        ipc_listener,
        ipc_tx,
        ipc_rx,
        hotkey_tx,
        hotkey_rx,
        wake_tx.clone(),
    );
    if let Err(e) = tay_window::run("tay", app, (wake_tx, wake_rx), output.as_deref()) {
        log::error!("事件循环退出: {e}");
    }
}
