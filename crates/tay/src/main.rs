//! tay — Tay CLI 薄客户端(DESIGN §4.1)。
//!
//! `tay [toggle|show|hide|quit]`(默认 toggle):连 daemon 发命令即退出。
//! daemon 不在时自动拉起(cold start 降级路径,§4.3)。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

fn socket_path() -> PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    dir.join("tay.sock")
}

fn send(cmd: &str) -> Result<String, String> {
    let mut stream =
        UnixStream::connect(socket_path()).map_err(|_| "daemon 未运行".to_string())?;
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
fn spawn_daemon() {
    let exe = std::env::current_exe().ok();
    let candidates: Vec<&str> = if let Some(exe) = &exe {
        if let Some(dir) = exe.parent() {
            let p = dir.join("tayd");
            if p.exists() {
                return spawn(&p);
            }
        }
        vec!["tayd"]
    } else {
        vec!["tayd"]
    };
    for c in candidates {
        if let Ok(out) = Command::new(c).arg("--daemon").spawn() {
            let _ = out;
            return;
        }
    }
    eprintln!("tay: 找不到 tayd,无法拉起 daemon");
    std::process::exit(1);
}

fn spawn(path: &std::path::Path) {
    let _ = Command::new(path).spawn();
}

/// `tay config`:生成默认配置(缺失时)+ 显示合成器热键片段。
fn cmd_config() {
    let path = config_path();
    let created = if path.exists() {
        false
    } else {
        let default = r##"# Tay 配置(全部可选,保存后 2 秒内热重载)
# 注意:顶层键需在 [font]/[colors] 表之前(TOML 语法要求)。

shell = ""            # 空 = $SHELL
height_percent = 45   # 呼出高度占输出百分比
animation_ms = 150    # 滑入动画时长,0 = 关闭
hide_on_focus_out = true
start_hidden = true
output = ""           # 呼出输出名(如 "DP-1");空 = 合成器默认

[font]
family = "JetBrainsMono Nerd Font"
size = 12.0

[colors]
foreground = "#d4d4d4"
background = "#0f1114"
cursor = "#00a884"
selection = "#395e9c"
"##;
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
    println!("Wayland:      {}", if wayland { "✓" } else { "✗(Tay 需要 Wayland)" });
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    println!("桌面:         {desktop}");
    println!("配置:         {}", config_path().display());
    // 字体。
    let font_ok = Command::new("fc-match")
        .arg("monospace")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    println!("等宽字体:     {}", if font_ok { "✓(fontconfig)" } else { "?" });
    // daemon 状态。
    let daemon = UnixStream::connect(socket_path()).is_ok();
    println!("daemon:       {}", if daemon { "✓ 运行中" } else { "未运行(tay toggle 会自动拉起)" });
    // portal。
    let portal = Command::new("gdbus")
        .args(["call", "--session", "--dest", "org.freedesktop.portal.Desktop",
               "--object-path", "/org/freedesktop/portal/desktop",
               "--method", "org.freedesktop.DBus.Properties.Get",
               "org.freedesktop.portal.GlobalShortcuts", "version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    println!("热键 portal:   {}", if portal { "✓(F12 首次需系统确认)" } else { "不可用(见下)" });
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
    println!("    <keybind key=\"F12\"><action name=\"Execute\" command=\"tay toggle\" /></keybind>");
}

fn config_path() -> std::path::PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME").map(|h| std::path::PathBuf::from(h).join(".config")).unwrap_or_default()
        })
        .join("tay")
        .join("config.toml")
}

fn main() {
    let cmd = std::env::args().nth(1).unwrap_or_else(|| "toggle".into());
    let cmd = match cmd.as_str() {
        "toggle" | "show" | "hide" | "quit" => cmd,
        "config" => {
            cmd_config();
            return;
        },
        "doctor" => {
            cmd_doctor();
            return;
        },
        "--daemon" => return, // 被 CLI 拉起时:正常初始化 daemon
        _ => {
            eprintln!("用法: tay [toggle|show|hide|quit|config|doctor](默认 toggle)");
            std::process::exit(2);
        },
    };

    match send(&cmd) {
        Ok(resp) => {
            if resp != "ok" {
                eprintln!("tay: {resp}");
                std::process::exit(1);
            }
        },
        Err(_) => {
            // 冷启动:拉起 daemon 后重试(§4.3 降级路径)。
            spawn_daemon();
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                std::thread::sleep(Duration::from_millis(50));
                if let Ok(resp) = send(&cmd) {
                    if resp != "ok" {
                        eprintln!("tay: {resp}");
                        std::process::exit(1);
                    }
                    return;
                }
                if Instant::now() > deadline {
                    eprintln!("tay: daemon 启动超时");
                    std::process::exit(1);
                }
            }
        },
    }
}
