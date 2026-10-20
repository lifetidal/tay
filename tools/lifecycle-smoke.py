#!/usr/bin/env python3
"""Isolated Wayland + transient systemd-user stop/resurrect regression.

Run after cargo build --workspace. Creates only a uniquely named transient unit,
configuration, socket and state under /tmp; never installs or stops tayd.service.
May briefly show a test window. Requires a running Wayland/user-systemd session.
"""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import tomllib

root = Path(__file__).resolve().parents[1]
base = Path(tempfile.mkdtemp(prefix="tay-lifecycle-"))
runtime = base / "runtime"
runtime.mkdir(mode=0o700)
config = base / "config" / "tay"
config.mkdir(parents=True)
(config / "config.lua").write_text('return { shell="/bin/sh", start_hidden=true, hide_on_focus_out=false, animation_ms=0 }\n')
wayland = Path(os.environ.get("WAYLAND_DISPLAY", "wayland-0"))
if not wayland.is_absolute():
    wayland = Path(os.environ["XDG_RUNTIME_DIR"]) / wayland
unit = f"tay-lifecycle-{os.getpid()}.service"
settings = {
    "XDG_RUNTIME_DIR": str(runtime), "XDG_CONFIG_HOME": str(base / "config"),
    "XDG_STATE_HOME": str(base / "state"), "WAYLAND_DISPLAY": str(wayland),
    "RUST_LOG": "info", "TAY_SHOT": f"{base / 'unused.png'},1000000",
}
env = dict(os.environ, **settings)
# Avoid unrelated development hooks inherited from the invoking shell.
for key in list(env):
    if key.startswith("TAY_") and key not in settings:
        del env[key]
cli = str(root / "target" / "debug" / "tay")
state = base / "state" / "tay" / "session.toml"


def command(*args, **kwargs):
    return subprocess.run(args, check=True, capture_output=True, text=True, timeout=15, **kwargs)


def wait_for(check, label):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        if check():
            return
        time.sleep(0.05)
    raise AssertionError(f"Timed out: {label}; inspect {base}")


def ready():
    try:
        return subprocess.run([cli, "show"], env=env, capture_output=True, timeout=2).returncode == 0
    except subprocess.TimeoutExpired:
        return False


try:
    args = ["systemd-run", "--user", "--quiet", "--collect", f"--unit={unit}",
            "--property=KillMode=mixed", "--property=TimeoutStopSec=5", "--property=WorkingDirectory=/tmp",
            f"--property=StandardError=append:{base / 'service.log'}"]
    args += [f"--setenv={key}={value}" for key, value in settings.items()]
    args += [str(root / "target" / "debug" / "tayd")]
    command(*args)
    # Do not use `tay show` to probe before the service is bound: the CLI would
    # cold-start a second daemon. Test socket existence, then allow IPC readiness.
    wait_for(lambda: (runtime / "tay.sock").exists(), "service socket")
    wait_for(ready, "service ready")
    for action in ["new-tab", "new-tab", "next-tab", "next-tab"]:
        command(cli, action, env=env)
        time.sleep(0.15)
    command("systemctl", "--user", "stop", unit)
    saved = tomllib.loads(state.read_text())
    assert len(saved["tabs"]) == 3, saved
    assert saved["active"] == 1, saved
    assert all(tab["cwd"] == "/tmp" for tab in saved["tabs"]), saved
    service_log = (base / "service.log").read_text()
    assert "cleanup timed out" not in service_log, service_log
    assert "panicked at" not in service_log, service_log
    command(cli, "resurrect", env=env)
    restored_log = runtime / "tayd.log"
    wait_for(lambda: restored_log.exists() and "恢复 3 个标签,激活 1" in restored_log.read_text(), "restore all tabs and active index")
    command(cli, "quit", env=env)
    wait_for(lambda: "会话文件已保存: 3 个标签" in restored_log.read_text(), "IPC quit persistence")
    print(json.dumps({"directory": str(base), "systemd_stop": "passed", "resurrect": "passed", "tabs": 3, "active": 1}, ensure_ascii=False))
finally:
    subprocess.run(["systemctl", "--user", "stop", unit], capture_output=True, timeout=15)
    subprocess.run([cli, "quit"], env=env, capture_output=True, timeout=15)
