#!/usr/bin/env python3
"""Wayland smoke test with isolated configuration and socket (no physical keys).
Run after `cargo build --workspace`. Opens a temporary window and keeps a screenshot.
"""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time

root = Path(__file__).resolve().parents[1]
base = Path(tempfile.mkdtemp(prefix="tay-usability-"))
runtime = base / "runtime"
runtime.mkdir(mode=0o700)
wayland = os.environ.get("WAYLAND_DISPLAY", "wayland-0")
if not os.path.isabs(wayland):
    wayland = str(Path(os.environ["XDG_RUNTIME_DIR"]) / wayland)
shot = base / "preview.png"
config = base / "config" / "tay"
config.mkdir(parents=True)
cfg = config / "config.lua"
log_path = runtime / "tayd.log"


def write_script(text):
    temporary = cfg.with_suffix(".tmp")
    temporary.write_text(text)
    temporary.replace(cfg)


def write_config(size):
    write_script(f'''-- Keep handwritten logic and comments.
local size = {size}
return {{
  shell = "/bin/sh", start_hidden = false, hide_on_focus_out = false,
  animation_ms = 0,
  font = {{ family = "monospace", size = size }},
  colors = {{ background = "#172a22", foreground = "#e8dbc1" }},
}}
''')


def log_text():
    return log_path.read_text() if log_path.exists() else ""


def wait_for(predicate, description, timeout=10):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.05)
    raise AssertionError(f"Timed out: {description}\n{log_text()}")


write_config(12)
appearance = config / "appearance.toml"
appearance.write_text('[font]\nfamily = "monospace"\nsize = 14.0\n')
env = dict(os.environ, XDG_RUNTIME_DIR=str(runtime), XDG_CONFIG_HOME=str(base / "config"),
           WAYLAND_DISPLAY=wayland, RUST_LOG="info", TAY_SHOT=f"{shot},250",
           TAY_SHELL="/bin/sh", TAY_EXEC="printf 'ASCII 012345 | 中文终端\\n'\n")
cli = str(root / "target/debug/tay")
results = []


def run(command):
    result = subprocess.run([cli, command], env=env, capture_output=True, text=True, timeout=15)
    results.append({"command": command, "returncode": result.returncode, "error": result.stderr})
    assert result.returncode == 0, results[-1]


try:
    run("quit")
    assert not (runtime / "tay.sock").exists()
    run("toggle")
    wait_for(lambda: "14pt" in log_text(), "startup appearance override")
    wait_for(lambda: not appearance.exists() and (config / "appearance.toml.migrated.bak").exists(), "legacy appearance archived")
    assert "__tay_config.font.size = 14" in cfg.read_text()
    write_config(12)
    wait_for(lambda: "网格调整" in log_text(), "Lua replaces migrated settings")
    before = log_text().count("网格调整")
    run("maximize")
    wait_for(lambda: log_text().count("网格调整") > before, "maximize resize")
    before = log_text().count("网格调整")
    run("restore")
    wait_for(lambda: log_text().count("网格调整") > before, "restore resize")
    for command in ["new-tab", "new-tab", "next-tab", "close-tab", "next-tab", "settings"]:
        run(command)
        time.sleep(0.1)
    run("close-tab")
    run("close-tab")
    time.sleep(0.2)
    assert (runtime / "tay.sock").exists(), "last tab killed daemon"
    run("show")
    run("new-tab")
    run("settings")
    before = log_text().count("网格调整")
    write_config(16)
    wait_for(lambda: log_text().count("网格调整") > before, "Lua font hot reload")
    write_script("while true do end")
    wait_for(lambda: "配置超出执行预算" in log_text(), "reject infinite Lua loop")
    run("next-tab")  # IPC/UI remain responsive after the rejected configuration.
    before = log_text().count("网格调整")
    write_config(12)
    wait_for(lambda: log_text().count("网格调整") > before, "recover valid configuration")
    wait_for(lambda: shot.exists() and shot.stat().st_size > 0 and "截图已保存" in log_text(), "completed screenshot", timeout=55)
finally:
    subprocess.run([cli, "quit"], env=env, capture_output=True, timeout=15)

log = log_text()
assert log.count("tayd 启动:") == 1, "unexpected daemon restart"
assert "panicked at" not in log, log
assert shot.stat().st_size > 0
print(json.dumps({"directory": str(base), "results": results, "screenshot": str(shot)}, ensure_ascii=False))
print(log[-6000:])
