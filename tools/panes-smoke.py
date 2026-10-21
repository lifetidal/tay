#!/usr/bin/env python3
"""Isolated native-pane Wayland regression; never touches the normal daemon.
Run after cargo build --workspace. Synthetic pointer/IME events use the app's
normal handler, not physical desktop input. Artifacts stay under /tmp.
"""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import tomllib

root = Path(__file__).resolve().parents[1]
base = Path(tempfile.mkdtemp(prefix="tay-panes-"))
runtime = base / "runtime"
runtime.mkdir(mode=0o700)
config = base / "config" / "tay"
config.mkdir(parents=True)
(config / "config.lua").write_text('return { shell="/bin/sh", start_hidden=false, hide_on_focus_out=false, height_percent=65, animation_ms=0, font={family="monospace",size=12} }\n')
wayland = Path(os.environ.get("WAYLAND_DISPLAY", "wayland-0"))
if not wayland.is_absolute():
    wayland = Path(os.environ["XDG_RUNTIME_DIR"]) / wayland
probe = base / "probe.toml"
shot = base / "panes.png"
env = {k: v for k, v in os.environ.items() if not k.startswith("TAY_")}
env.update(XDG_RUNTIME_DIR=str(runtime), XDG_CONFIG_HOME=str(base / "config"),
           XDG_STATE_HOME=str(base / "state"), WAYLAND_DISPLAY=str(wayland),
           TAY_PANES_SMOKE=str(probe), TAY_SHOT=f"{shot},500", RUST_LOG="info")
cli = str(root / "target/debug/tay")
daemon = str(root / "target/debug/tayd")
log_path = base / "first.log"
process = None


def wait_for(check, label, timeout=20):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if check():
            return
        if process is not None and process.poll() is not None:
            raise AssertionError(f"Daemon exited during {label}: {log_path.read_text()}")
        time.sleep(0.05)
    raise AssertionError(f"Timed out: {label}; {log_path.read_text()}")


def run(action):
    result = subprocess.run([cli, action], env=env, capture_output=True, text=True, timeout=15)
    assert result.returncode == 0, (action, result.stderr)
    time.sleep(0.12)


def leaf_count(layout):
    return 1 if layout["kind"] == "leaf" else leaf_count(layout["first"]) + leaf_count(layout["second"])


def saved():
    return tomllib.loads((base / "state/tay/session.toml").read_text())


try:
    with log_path.open("w") as log:
        process = subprocess.Popen([daemon], env=env, cwd=base, stdout=log, stderr=log)
    wait_for(lambda: (runtime / "tay.sock").exists(), "startup")
    run("split-right")
    run("split-down")
    wait_for(probe.exists, "pointer drag, focus, IME, independent input")
    assert tomllib.loads(probe.read_text())["input_isolated"]
    # Preserve a screenshot of the actual composed pane view before testing zoom.
    wait_for(shot.exists, "split screenshot", timeout=30)
    # Screenshot mode exits after saving; graceful quit before that would lose the
    # artifact. Start a second isolated run for persistence if it has exited.
    process.wait(timeout=10)
    env["TAY_PANES_SMOKE"] = str(base / "probe-second.toml")
    env["TAY_SHOT"] = f"{base / 'unused.png'},1000000"
    log_path = base / "second.log"
    with log_path.open("w") as log:
        process = subprocess.Popen([daemon], env=env, cwd=base, stdout=log, stderr=log)
    wait_for(lambda: "初始" in log_path.read_text() or "网格 " in log_path.read_text(), "second startup")
    run("split-right")
    run("split-down")
    wait_for(lambda: (base / "probe-second.toml").exists(), "second-run drag")
    run("zoom-pane")
    run("zoom-pane")
    run("next-pane")
    run("new-tab")
    run("next-tab")
    run("quit")
    assert process.wait(timeout=10) == 0
    data = saved()
    assert data["version"] == 2 and len(data["tabs"]) == 2, data
    assert leaf_count(data["tabs"][0]["layout"]) == 3, data
    assert data["tabs"][0]["focused"] == 1, data
    before = data["tabs"][0]["layout"]
    assert before["ratio"] > 500, before
    env.pop("TAY_PANES_SMOKE", None)
    env["TAY_RESURRECT"] = "1"
    log_path = base / "restore.log"
    with log_path.open("w") as log:
        process = subprocess.Popen([daemon], env=env, cwd=base, stdout=log, stderr=log)
    wait_for(lambda: "恢复 2 个标签,激活 0" in log_path.read_text(), "layout restore")
    run("quit")
    assert process.wait(timeout=10) == 0
    restored = saved()
    assert restored["tabs"][0]["layout"] == before, restored
    assert restored["tabs"][0]["focused"] == 1, restored
    log_path = base / "close.log"
    with log_path.open("w") as log:
        process = subprocess.Popen([daemon], env=env, cwd=base, stdout=log, stderr=log)
    wait_for(lambda: "恢复 2 个标签,激活 0" in log_path.read_text(), "second restore")
    run("close-pane")
    run("quit")
    assert process.wait(timeout=10) == 0
    after = saved()
    assert len(after["tabs"]) == 2 and leaf_count(after["tabs"][0]["layout"]) == 2, after
    assert "panicked at" not in log_path.read_text()
    print(json.dumps({"directory": str(base), "screenshot": str(shot), "probe": "passed", "restore": "passed", "close_pane": "passed", "saved_layout": before}, ensure_ascii=False))
finally:
    if process is not None and process.poll() is None:
        subprocess.run([cli, "quit"], env=env, capture_output=True, timeout=15)
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
