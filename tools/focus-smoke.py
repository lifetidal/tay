#!/usr/bin/env python3
"""Verify focus returns on hide using an isolated virtual KWin and Qt client.

Requires kwin_wayland, dbus-run-session, Qt6 qml and built tay/tayd binaries.
No host desktop access or synthetic input. Override TAY_TEST_BIN_DIR to test
an extracted package. Logs and configuration stay in a unique /tmp directory.
"""
import os
from pathlib import Path
import re
import signal
import subprocess
import tempfile
import time

root = Path(__file__).resolve().parents[1]
binaries = Path(os.environ.get('TAY_TEST_BIN_DIR', root / 'target/debug'))
base = Path(tempfile.mkdtemp(prefix='tay-focus-'))
runtime = base / 'runtime'
runtime.mkdir(mode=0o700)
env = dict(os.environ, XDG_RUNTIME_DIR=str(runtime),
           XDG_CONFIG_HOME=str(base / 'config'), XDG_CACHE_HOME=str(base / 'cache'),
           XDG_STATE_HOME=str(base / 'state'), QT_QPA_PLATFORM='offscreen',
           KWIN_COMPOSE='O2')
for key in list(env):
    if key.startswith('TAY_') or key in ('WAYLAND_DISPLAY', 'DISPLAY', 'SESSION_MANAGER'):
        del env[key]
processes = []
handles = []


def launch(args, environment, logfile):
    handle = logfile.open('w')
    handles.append(handle)
    process = subprocess.Popen(args, env=environment, stdout=handle, stderr=handle,
                               start_new_session=True)
    processes.append(process)
    return process


def wait_for(check, description):
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        if check():
            return
        time.sleep(.05)
    raise AssertionError(f'{description}; logs: {base}')


def text(path):
    return path.read_text(errors='replace') if path.exists() else ''


def events(path, event):
    return len(re.findall(r'wl_keyboard[#@]\d+\.' + event + r'\(', text(path)))


try:
    launch(['dbus-run-session', '--', 'kwin_wayland', '--virtual', '--width', '1280',
            '--height', '800', '--no-lockscreen', '--no-global-shortcuts',
            '--no-kactivities', '--socket', 'tay-test'], env, base / 'kwin.log')
    wait_for(lambda: (runtime / 'tay-test').exists(), 'virtual compositor startup')
    qml = base / 'probe.qml'
    qml.write_text('import QtQuick\nimport QtQuick.Window\n'
                   'Window { visible: true; width: 600; height: 400; '
                   'title: "Tay Focus Regression"; '
                   'TextInput { anchors.fill: parent; focus: true; text: "focus probe" } }\n')
    client_env = dict(env, WAYLAND_DISPLAY=str(runtime / 'tay-test'),
                      QT_QPA_PLATFORM='wayland', WAYLAND_DEBUG='1',
                      DBUS_SESSION_BUS_ADDRESS=os.environ.get('TAY_TEST_DESKTOP_BUS', 'unix:path=' + str(base / 'no-bus')))
    probe_log = base / 'probe.log'
    launch(['/usr/lib/qt6/bin/qml', str(qml)], client_env, probe_log)
    wait_for(lambda: events(probe_log, 'enter') > 0, 'baseline client initial focus')
    for duration in (0, 150):
        case = base / str(duration)
        case_runtime = case / 'runtime'
        case_runtime.mkdir(parents=True, mode=0o700)
        config = case / 'config/tay'
        config.mkdir(parents=True)
        (config / 'config.lua').write_text(
            f'return {{shell="/bin/sh",start_hidden=true,hide_on_focus_out=false,'
            f'animation_ms={duration}}}\n')
        tay_env = dict(client_env, XDG_RUNTIME_DIR=str(case_runtime),
                       XDG_CONFIG_HOME=str(case / 'config'), XDG_STATE_HOME=str(case / 'state'),
                       RUST_LOG='info')
        daemon_log = case / 'tayd.log'
        daemon = launch([str(binaries / 'tayd')], tay_env, daemon_log)
        wait_for(lambda: (case_runtime / 'tay.sock').exists(), 'Tay IPC socket')
        # Wait for GPU and PTY initialization; do not let CLI cold-start another daemon.
        wait_for(lambda: '网格 ' in text(daemon_log) or daemon.poll() is not None,
                 'Tay renderer/PTY initialization')
        assert daemon.poll() is None, text(daemon_log)[-3000:]
        if os.environ.get('TAY_TEST_USE_PORTAL'):
            wait_for(lambda: 'session=' in text(daemon_log) and '全局快捷键：' in text(daemon_log), 'portal binding')
            wait_for(lambda: text(daemon_log).count('全局快捷键触发：toggle') >= 20,
                     '20 buffered press/release pairs must produce 20 toggles')
            print('PASS: all 20 buffered shortcut presses delivered in order', flush=True)

        def command(action):
            if os.environ.get('TAY_TEST_USE_PORTAL') and action in ('show', 'hide', 'toggle'):
                subprocess.run(['gdbus', 'call', '--session', '--dest', 'org.freedesktop.portal.Desktop',
                                '--object-path', '/org/freedesktop/portal/desktop', '--method', 'org.tay.Test.Pulse'],
                               env=tay_env, check=True, capture_output=True, timeout=10)
                return
            subprocess.run([str(binaries / 'tay'), action], env=tay_env,
                           check=True, capture_output=True, timeout=10)

        for action in ('hide', 'toggle', 'hide'):
            before_leave = events(probe_log, 'leave')
            command('show')
            wait_for(lambda: events(probe_log, 'leave') > before_leave,
                     f'{duration}ms: Tay acquires focus')
            before_enter = events(probe_log, 'enter')
            command(action)
            wait_for(lambda: events(probe_log, 'enter') > before_enter,
                     f'{duration}ms: focus returns automatically after {action}')
            assert daemon.poll() is None, text(daemon_log)[-3000:]
        if os.environ.get('TAY_TEST_USE_PORTAL') and duration == 0:
            bindings = text(daemon_log).count('session=')
            subprocess.run(['gdbus', 'call', '--session', '--dest', 'org.freedesktop.portal.Desktop',
                            '--object-path', '/org/freedesktop/portal/desktop', '--method', 'org.tay.Test.ResetSession'],
                           env=tay_env, check=True, capture_output=True, timeout=10)
            wait_for(lambda: text(daemon_log).count('session=') > bindings, 'shortcut reconnect after Closed')
            before_leave = events(probe_log, 'leave')
            command('show')
            wait_for(lambda: events(probe_log, 'leave') > before_leave, 'reconnected shortcut shows Tay')
            before_enter = events(probe_log, 'enter')
            command('hide')
            wait_for(lambda: events(probe_log, 'enter') > before_enter, 'reconnected shortcut hides Tay')
            print('PASS: shortcut session reconnect', flush=True)
        command('quit')
        daemon.wait(timeout=10)
        print(f'PASS animation_ms={duration}: hide/toggle/remap restore focus', flush=True)
    print(f'PASS; logs: {base}', flush=True)
finally:
    for process in reversed(processes):
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=5)
    for handle in handles:
        handle.close()
