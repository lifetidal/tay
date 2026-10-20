//! tay — Tay CLI 薄客户端(DESIGN §4.1)。
//!
//! `tay [toggle|show|hide|quit]`(默认 toggle):连 daemon 发命令即退出。
//! daemon 不在时自动拉起(cold start 降级路径,§4.3)。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn socket_path() -> PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    dir.join("tay.sock")
}

/// 会话文件路径 —— 与 daemon 的 persist::path() 保持一致
/// ($XDG_STATE_HOME || ~/.local/state)/tay/session.toml。
fn session_file() -> PathBuf {
    let base = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .ok()
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".local/state"))
        })
        .unwrap_or_else(std::env::temp_dir);
    base.join("tay").join("session.toml")
}

fn send(cmd: &str) -> Result<String, String> {
    let mut stream = UnixStream::connect(socket_path()).map_err(|_| "daemon 未运行".to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .map_err(|e| e.to_string())?;
    writeln!(stream, "{cmd}").map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    Ok(line.trim().to_string())
}

/// 拉起 daemon(tayd 需在 PATH 或与 tay 同目录)。
/// `resurrect` = 携带 TAY_RESURRECT=1:daemon 启动时按会话文件
/// 恢复上次退出时保存的标签骨架。
fn spawn_daemon(resurrect: bool) -> Result<Child, String> {
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("tayd")))
        .filter(|p| p.exists());
    let path = sibling.unwrap_or_else(|| PathBuf::from("tayd"));
    let log_path = socket_path().with_file_name("tayd.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("无法创建日志 {}: {e}", log_path.display()))?;
    let stderr = log.try_clone().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(&path);
    if resurrect {
        cmd.env("TAY_RESURRECT", "1");
    }
    cmd.process_group(0)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(stderr)
        .spawn()
        .map_err(|e| format!("无法启动 {}: {e}", path.display()))
}

/// `tay config`:生成默认配置(缺失时)+ 显示合成器热键片段。
fn cmd_config() {
    let path = tay_config::Config::config_path();
    let created = if path.exists() {
        false
    } else {
        let default = tay_config::Config::default().to_lua();
        std::fs::create_dir_all(path.parent().unwrap()).ok();
        std::fs::write(&path, default).is_ok()
    };
    if created {
        println!("已生成默认配置: {}", path.display());
    } else {
        println!("配置已存在: {}", path.display());
    }
    println!();
    print_hotkey_snippets();
}

/// `tay doctor`:环境与运行状态检查。
fn cmd_doctor() {
    println!("== Tay 环境检查 ==");
    let wayland = std::env::var("WAYLAND_DISPLAY").is_ok();
    println!(
        "Wayland:      {}",
        if wayland {
            "✓"
        } else {
            "✗(Tay 需要 Wayland)"
        }
    );
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    println!("桌面:         {desktop}");
    println!("配置:         {}", config_path().display());
    // 字体。
    let font_ok = Command::new("fc-match")
        .arg("monospace")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    println!(
        "等宽字体:     {}",
        if font_ok { "✓(fontconfig)" } else { "?" }
    );
    // daemon 状态。
    let daemon = UnixStream::connect(socket_path()).is_ok();
    println!(
        "daemon:       {}",
        if daemon {
            "✓ 运行中"
        } else {
            "未运行(tay toggle 会自动拉起)"
        }
    );
    // portal。
    let portal = Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.freedesktop.portal.Desktop",
            "--object-path",
            "/org/freedesktop/portal/desktop",
            "--method",
            "org.freedesktop.DBus.Properties.Get",
            "org.freedesktop.portal.GlobalShortcuts",
            "version",
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    // 热键按配置显示(config.lua 的 global_shortcut),不再是硬编码 F12。
    let shortcut = tay_config::Config::load().global_shortcut;
    println!(
        "热键 portal:   {}",
        if portal {
            format!("✓({shortcut} 首次需系统确认)")
        } else {
            "不可用(见下)".into()
        }
    );
    if !portal {
        println!();
        print_hotkey_snippets();
    }
}

/// 合成器热键绑定片段(portal 不可用时的引导,§6.1)。
fn print_hotkey_snippets() {
    println!("== 热键绑定(合成器侧绑定 tay toggle)==");
    println!("sway / river:");
    println!("    bindsym F12 exec tay toggle");
    println!("niri:");
    println!("    binds Mod+F12 spawn tay toggle");
    println!("Hyprland:");
    println!("    bind = ,F12, exec, tay toggle");
    println!("labwc:");
    println!(
        "    <keybind key=\"F12\"><action name=\"Execute\" command=\"tay toggle\" /></keybind>"
    );
}

fn config_path() -> std::path::PathBuf {
    tay_config::Config::config_path()
}

fn cmd_keys() -> std::io::Result<()> {
    let config = tay_config::Config::try_load()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "配置: {}", config_path().display())?;
    writeln!(
        out,
        "全局呼出请求: {} (以桌面实际授权为准)",
        config.global_shortcut
    )?;
    for binding in &config.keys {
        writeln!(
            out,
            "{:<16} {:<12} {}",
            binding.mods, binding.key, binding.action
        )?;
    }
    Ok(())
}

// The daemon intentionally outlives this short-lived client; on client exit
// it is reparented to the system's process reaper. Waiting would hang `tay`.
#[allow(clippy::zombie_processes)]
fn main() {
    let cmd = std::env::args().nth(1).unwrap_or_else(|| "toggle".into());
    let cmd = match cmd.as_str() {
        "toggle" | "show" | "hide" | "quit" | "pin" | "new-tab" | "next-tab" | "close-tab"
        | "settings" | "maximize" | "restore" => cmd,
        "config" => {
            cmd_config();
            return;
        }
        "keys" => {
            if let Err(e) = cmd_keys()
                && e.kind() != std::io::ErrorKind::BrokenPipe
            {
                eprintln!("tay keys: {e}");
                std::process::exit(1);
            }
            return;
        }
        "doctor" => {
            cmd_doctor();
            return;
        }
        "resurrect" => {
            // 恢复上次退出保存的标签骨架:要求 daemon 未运行
            // (quit 时才写会话文件;运行中恢复会与现有会话冲突)。
            if UnixStream::connect(socket_path()).is_ok() {
                eprintln!(
                    "tay: daemon 已在运行 —— 请先 tay quit(退出时写入会话文件),再 tay resurrect"
                );
                std::process::exit(1);
            }
            if !session_file().exists() {
                eprintln!("tay: 没有可恢复的会话文件(上次退出时未保存);tay toggle 正常启动");
                std::process::exit(1);
            }
            let mut child = spawn_daemon(true).unwrap_or_else(|e| {
                eprintln!("tay: {e}");
                std::process::exit(1);
            });
            // 与冷启动同一就绪协议:等 socket 应答 show。
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                std::thread::sleep(Duration::from_millis(50));
                if let Ok(resp) = send("show") {
                    if resp != "ok" {
                        eprintln!("tay: {resp}");
                        std::process::exit(1);
                    }
                    return;
                }
                if let Ok(Some(status)) = child.try_wait()
                    && !status.success()
                {
                    eprintln!(
                        "tay: daemon 启动失败({status}); 日志: {}",
                        socket_path().with_file_name("tayd.log").display()
                    );
                    std::process::exit(1);
                }
                if Instant::now() > deadline {
                    eprintln!(
                        "tay: daemon 启动超时; 日志: {}",
                        socket_path().with_file_name("tayd.log").display()
                    );
                    std::process::exit(1);
                }
            }
        }
        "--daemon" => return, // 被 CLI 拉起时:正常初始化 daemon
        _ => {
            eprintln!(
                "用法: tay [toggle|show|hide|quit|pin|new-tab|next-tab|close-tab|settings|maximize|restore|resurrect|config|keys|doctor](默认 toggle)"
            );
            std::process::exit(2);
        }
    };

    match send(&cmd) {
        Ok(resp) => {
            if resp != "ok" {
                eprintln!("tay: {resp}");
                std::process::exit(1);
            }
        }
        Err(_) => {
            // 冷启动:拉起 daemon 后重试(§4.3 降级路径)。
            if matches!(cmd.as_str(), "hide" | "quit") {
                return;
            }
            let mut child = spawn_daemon(false).unwrap_or_else(|e| {
                eprintln!("tay: {e}");
                std::process::exit(1);
            });
            // A cold toggle means show, even when start_hidden=false.
            let startup_cmd = if cmd == "toggle" { "show" } else { &cmd };
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                std::thread::sleep(Duration::from_millis(50));
                if let Ok(resp) = send(startup_cmd) {
                    if resp != "ok" {
                        eprintln!("tay: {resp}");
                        std::process::exit(1);
                    }
                    return;
                }
                if let Ok(Some(status)) = child.try_wait() {
                    // A competing launcher can win the single-instance lock.
                    if !status.success() {
                        eprintln!(
                            "tay: daemon 启动失败({status}); 日志: {}",
                            socket_path().with_file_name("tayd.log").display()
                        );
                        std::process::exit(1);
                    }
                }
                if Instant::now() > deadline {
                    eprintln!(
                        "tay: daemon 启动超时; 日志: {}",
                        socket_path().with_file_name("tayd.log").display()
                    );
                    std::process::exit(1);
                }
            }
        }
    }
}
