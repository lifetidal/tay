#!/usr/bin/env python3
"""Exercise scrollbar pointer dispatch in an isolated Wayland/PTY instance.

The TAY_SCROLLBAR hook dispatches PointerButton/PointerMoved through the app's
normal handler; it does not inject physical desktop input.
"""
import json
import os
from pathlib import Path
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
base = Path(tempfile.mkdtemp(prefix='tay-scrollbar-'))
runtime = base / 'runtime'
runtime.mkdir(mode=0o700)
config = base / 'config/tay'
config.mkdir(parents=True)
(config / 'config.lua').write_text('''return {
  shell='/bin/sh', start_hidden=false, hide_on_focus_out=false,
  animation_ms=0, theme='midnight', padding=10,
}''')
wayland = os.environ.get('WAYLAND_DISPLAY', 'wayland-0')
if not os.path.isabs(wayland):
    wayland = str(Path(os.environ['XDG_RUNTIME_DIR']) / wayland)
env = {k: v for k, v in os.environ.items() if not k.startswith('TAY_')}
env.update(XDG_CONFIG_HOME=str(base / 'config'), XDG_RUNTIME_DIR=str(runtime),
           WAYLAND_DISPLAY=wayland, TAY_SCROLLBAR='1',
           TAY_EXEC='seq 1 320', TAY_SHOT=f'{base}/scrollbar.png,240', RUST_LOG='info')
with (base / 'log').open('w') as log:
    proc = subprocess.Popen([str(root / 'target/debug/tayd')], env=env, stdout=log, stderr=log)
    try:
        proc.wait(timeout=25)
    finally:
        if proc.poll() is None:
            proc.terminate()
            proc.wait(timeout=5)
text = (base / 'log').read_text()
assert proc.returncode == 0, text
assert 'drag to oldest and release over header passed' in text, text
assert 'track click to latest passed' in text, text
assert 'panicked at' not in text, text
assert (base / 'scrollbar.png').exists(), text
print(json.dumps({'screenshot': str(base / 'scrollbar.png'), 'log': str(base / 'log')}))
