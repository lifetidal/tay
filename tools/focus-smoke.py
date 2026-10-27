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
           KWIN_COMPOSE='O2', KWIN_SCREENSHOT_NO_PERMISSION_CHECKS='1')
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
    busfile = base / 'compositor-bus'
    wrapper = 'import os,sys;from pathlib import Path;Path(sys.argv[1]).write_text(os.environ["DBUS_SESSION_BUS_ADDRESS"]);os.execvp(sys.argv[2],sys.argv[2:])'
    launch(['dbus-run-session', '--', '/usr/bin/python3', '-c', wrapper, str(busfile), 'kwin_wayland', '--virtual', '--width', '1280',
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
    def screenshot(name):
        path = base / (name + '.png')
        shot_env = dict(client_env, DBUS_SESSION_BUS_ADDRESS=busfile.read_text())
        subprocess.run(['spectacle', '--background', '--nonotify', '--fullscreen', '--output', str(path)],
                       env=shot_env, check=True, capture_output=True, timeout=15)
        import gi
        gi.require_version('GdkPixbuf', '2.0')
        from gi.repository import GdkPixbuf
        image = GdkPixbuf.Pixbuf.new_from_file(str(path))
        return bytes(image.get_pixels())

    baseline = screenshot('baseline')
    for duration in (0, 150):
        case = base / str(duration)
        case_runtime = case / 'runtime'
        case_runtime.mkdir(parents=True, mode=0o700)
        config = case / 'config/tay'
        config.mkdir(parents=True)
        (config / 'config.lua').write_text(
            f'return {{shell="/bin/sh",start_hidden=true,hide_on_focus_out=false,'
            f'animation_ms={duration},background_image={os.environ.get("TAY_TEST_BACKGROUND_IMAGE", "")!r},background_image_opacity=0.8,background_blur=true}}\n')
        tay_env = dict(client_env, XDG_RUNTIME_DIR=str(case_runtime),
                       XDG_CONFIG_HOME=str(case / 'config'), XDG_STATE_HOME=str(case / 'state'),
                       RUST_LOG='info')
        if os.environ.get('TAY_TEST_PRODUCT') and duration == 0:
            tay_env['TAY_EXEC'] = 'sleep 60\n'
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
            # 键盘焦点在动画开头就切换:等滑入动画完成再截,
            # 否则拍到的是滑入中的中间帧(成败取决于截图工具启动快慢)。
            time.sleep(duration / 1000 + 0.2)
            visible = screenshot(f'{duration}-{action}-visible')
            assert len(visible) == len(baseline), 'screenshot dimensions changed'
            assert sum(abs(a-b) > 30 for a,b in zip(baseline, visible)) > 15000, 'focus changed but Tay is not visible'
            before_enter = events(probe_log, 'enter')
            command(action)
            wait_for(lambda: events(probe_log, 'enter') > before_enter,
                     f'{duration}ms: focus returns automatically after {action}')
            # hide() 在动画开头就释放键盘(focus-enter 立刻触发),
            # 而 margin 滑出还要 animation_ms:等动画完成再截,
            # 否则会拍到窗口仍在屏幕上的帧,误报「隐藏后仍可见」。
            time.sleep(duration / 1000 + 0.2)
            hidden = screenshot(f'{duration}-{action}-hidden')
            assert sum(abs(a-b) > 30 for a,b in zip(baseline, hidden)) < 15000, 'Tay remained visible after hiding'
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
        if os.environ.get('TAY_TEST_PRODUCT') and duration == 0:
            import tomllib
            def status():
                result = subprocess.run([str(binaries / 'tay'), 'status'], env=tay_env,
                                        check=True, capture_output=True, text=True, timeout=5)
                return dict(line.split('=', 1) for line in result.stdout.splitlines() if '=' in line)
            command('show')
            wait_for(lambda: 'sleep 60' in text(daemon_log) or status().get('window') == '显示中', 'product test shell shown')
            dimensions=status().get('terminal_dims')
            command('settings')
            wait_for(lambda: status().get('settings')=='true','settings overlay opened')
            assert status().get('terminal_dims')==dimensions,'settings resized the PTY'
            time.sleep(0.25)
            screenshot('product-settings')
            for page,index in [('background','1'),('shortcuts','2'),('appearance','0')]:
                subprocess.run([str(binaries / 'tay'),'settings',page],env=tay_env,check=True,capture_output=True,timeout=10)
                wait_for(lambda: status().get('settings_page')==index,'settings page '+page)
                time.sleep(0.2)
                screenshot('settings-'+page)
                assert status().get('terminal_dims')==dimensions,'settings page resized the PTY'
            command('close-tab')
            wait_for(lambda: status().get('confirmation') == 'true', 'foreground task close confirmation')
            assert daemon.poll() is None, 'busy close killed daemon'
            screenshot('product-close-confirmation')
            command('cancel-close')
            wait_for(lambda: status().get('confirmation') == 'false', 'cancel retains live task')
            command('new-tab')
            state_file = case / 'state/tay/session.toml'
            def saved_two_tabs():
                try:
                    return len(tomllib.loads(state_file.read_text())['tabs']) == 2
                except (OSError, ValueError, KeyError):
                    return False
            wait_for(saved_two_tabs, 'layout saved while daemon is still running')
            recovery_fixture = state_file.read_text()
            command('close-tab')  # idle shell does not require confirmation
            wait_for(lambda: status().get('tabs') == '1', 'idle close without confirmation')
            command('close-tab')
            wait_for(lambda: status().get('confirmation') == 'true', 'busy close still protected')
            command('confirm-close')
            wait_for(lambda: status().get('confirmation') == 'false', 'explicit confirmation closes task')
            command('show')
            print('PASS: live diagnostics, foreground close/cancel/confirm, idle close, autosave and settings screenshot', flush=True)
        command('quit')
        daemon.wait(timeout=10)
        if os.environ.get('TAY_TEST_PRODUCT') and duration == 0:
            marker = state_file.with_suffix('.running')
            assert not marker.exists(), 'normal shutdown left recovery marker'
            state_file.write_text(recovery_fixture)
            marker.write_text('previous-crashed-process')
            recovered_env = dict(tay_env)
            recovered_env.pop('TAY_EXEC', None)
            recovered_log = case / 'recovered.log'
            daemon = launch([str(binaries / 'tayd')], recovered_env, recovered_log)
            wait_for(lambda: 'resurrect: 恢复 2 个标签' in text(recovered_log), 'unclean startup restores saved layout')
            wait_for(lambda: status().get('tabs') == '2', 'restored layout visible in live diagnostics')
            command('quit')
            daemon.wait(timeout=10)
            assert not marker.exists(), 'recovered normal shutdown left recovery marker'
            print('PASS: simulated unclean restart restores autosaved layout before overwriting it', flush=True)
            for page in ('appearance','background'):
                capture = base / ('tay-settings-' + page + '.png')
                capture_env = dict(recovered_env,TAY_START_VISIBLE='1',TAY_SHOT=str(capture)+',90')
                capture_log = case / ('capture-' + page + '.log')
                daemon = launch([str(binaries / 'tayd')],capture_env,capture_log)
                wait_for(lambda: '网格 ' in text(capture_log),'capture renderer ready')
                subprocess.run([str(binaries / 'tay'),'settings',page],env=capture_env,
                               check=True,capture_output=True,timeout=10)
                wait_for(lambda: capture.exists(),'standalone '+page+' screenshot')
                daemon.wait(timeout=10)
            print('PASS: standalone screenshots from package renderer',flush=True)

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
