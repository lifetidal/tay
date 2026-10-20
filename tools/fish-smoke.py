#!/usr/bin/env python3
"""Capture fish Unicode/Nerd Font rendering in an isolated Wayland instance.

Optional: --starship-config ~/.config/starship/starship.toml
No user fish configuration, history or universal variables are modified.
"""
import argparse
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--starship-config', type=Path)
parser.add_argument('--bin-dir', type=Path, default=root / 'target/debug')
args = parser.parse_args()
fish = shutil.which('fish')
if not fish:
    parser.error('fish is required')
if args.starship_config and not shutil.which('starship'):
    parser.error('starship is required with --starship-config')
if args.starship_config and not args.starship_config.is_file():
    parser.error('starship config does not exist')

base = Path(tempfile.mkdtemp(prefix='tay-fish-smoke-'))
runtime = base / 'runtime'
runtime.mkdir(mode=0o700)
config = base / 'config/tay'
config.mkdir(parents=True)
wrapper = base / 'fish-start'
init = ' --init-command ' + shlex.quote('starship init fish | source') if args.starship_config else ''
wrapper.write_text(f'#!/bin/sh\nexec {shlex.quote(fish)} --no-config{init}\n')
wrapper.chmod(0o755)
(config / 'config.lua').write_text(f'''return {{
  shell = "{wrapper}", theme = "wezterm", opacity = 1,
  font = {{family = "JetBrainsMono Nerd Font", size = 12, weight = 500}},
  height_percent = 55, animation_ms = 0, start_hidden = false, hide_on_focus_out = false,
}}
''')
wayland = os.environ.get('WAYLAND_DISPLAY', 'wayland-0')
if not os.path.isabs(wayland):
    wayland = str(Path(os.environ['XDG_RUNTIME_DIR']) / wayland)
shot = base / 'fish.png'
# Escape Unicode so fish's interactive key decoder is not the subject of this
# output-rendering test. Printed text still traverses the real PTY and parser.
text = "Nerd:       󰊢 | italic: \\033[3mfjÅé\\033[0m | combining: é ä | 中文"
text = ''.join(c if ord(c) < 128 else chr(92) + 'U' + format(ord(c), '08x') for c in text)
command = "printf '\\033[2J\\033[H" + text + "\\n'"
env = {k: v for k, v in os.environ.items() if not k.startswith('TAY_')}
env.update(XDG_CONFIG_HOME=str(base / 'config'), XDG_RUNTIME_DIR=str(runtime),
           WAYLAND_DISPLAY=wayland, TAY_SHOT=f'{shot},240', TAY_EXEC=command,
           RUST_LOG='info', STARSHIP_CACHE=str(base / 'starship-cache'))
env.pop('NO_COLOR', None)
if args.starship_config:
    env['STARSHIP_CONFIG'] = str(args.starship_config.resolve())
log_path = base / 'log'
with log_path.open('w') as log:
    child = subprocess.Popen([str(args.bin_dir.resolve() / 'tayd')], env=env,
                             cwd=root, stdout=log, stderr=log)
    try:
        code = child.wait(timeout=45)
    except subprocess.TimeoutExpired:
        child.terminate()
        child.wait(timeout=5)
        raise
assert code == 0, log_path.read_text()
assert shot.exists() and shot.stat().st_size > 0, log_path.read_text()
assert '截图已保存' in log_path.read_text(), log_path.read_text()
assert 'panicked at' not in log_path.read_text(), log_path.read_text()
print(json.dumps({'screenshot': str(shot), 'log': str(log_path)}, ensure_ascii=False))
