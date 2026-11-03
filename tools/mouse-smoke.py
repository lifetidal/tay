#!/usr/bin/env python3
"""Exercise mouse reporting end-to-end in an isolated Wayland/PTY instance.

TAY_MOUSE injects synthetic PointerButton/PointerMoved/Scroll through the
app's normal handler; the shell first enables 1000+1006 and drains the PTY
input with dd, so the tool can assert the exact SGR bytes arrived. It does
not inject physical desktop input.
"""
import json
import os
from pathlib import Path
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
base = Path(tempfile.mkdtemp(prefix='tay-mouse-'))
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
mouse_out = base / 'mouse.bin'
# 开启鼠标模式(1000 点击 + 1006 SGR 编码),然后 exec dd 收 41
# 字节(按下 9 + 松开 10 + 滚轮 22;TAY_MOUSE 注入的期望序列)。
# 注意:TAY_EXEC 是作为一整行喂给 shell 的,必须保持裸命令形态
# (shlex.quote 会把整行包成一个词);printf 带换行,pty 上 stdout
# 行缓冲,不带换行要等 shell 退出才 flush,模式开启序列到不了终端;
# stty raw 关掉 canonical 行规程 —— 鼠标字节不含换行,行规程会
# 把它们扣在缓冲区不交付(仓库 TAY_PASTE 验收同款);
# exec 让 shell 被 dd 替换,否则 shell 自己也阻塞在读 stdin
# 等下一行,与 dd 竞争同一 pty,鼠标字节流向是竞态。
script = "printf '\\033[?1000h\\033[?1006h\\n'; stty raw -echo; exec dd bs=1 count=41 of='" + str(mouse_out) + "'"
env = {k: v for k, v in os.environ.items() if not k.startswith('TAY_')}
env.update(XDG_CONFIG_HOME=str(base / 'config'), XDG_RUNTIME_DIR=str(runtime),
           WAYLAND_DISPLAY=wayland, TAY_MOUSE='1',
           TAY_EXEC=script, TAY_MOUSE_OUT=str(mouse_out),
           TAY_SHOT=f'{base}/mouse.png,240', RUST_LOG='info')
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
assert 'TAY_MOUSE:鼠标上报链路通过' in text, text
assert mouse_out.exists() and mouse_out.stat().st_size == 41, (text, mouse_out.stat().st_size if mouse_out.exists() else 'missing')
assert 'panicked at' not in text, text
assert (base / 'mouse.png').exists(), text
print(json.dumps({'screenshot': str(base / 'mouse.png'), 'log': str(base / 'log')}))
