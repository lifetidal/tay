#!/usr/bin/env python3
"""Check cursor, background-color and pixel-size replies through a real isolated PTY."""
import os,subprocess,tempfile,json
from pathlib import Path
root=Path(__file__).resolve().parents[1];base=Path(tempfile.mkdtemp(prefix='tay-query-review-'))
runtime=base/'runtime';runtime.mkdir(mode=0o700);config=base/'config/tay';config.mkdir(parents=True)
report=base/'responses.json';helper=base/'query.py'
helper.write_text(f'''import os,tty,termios,time,select,json
from pathlib import Path
time.sleep(0.15)
saved=termios.tcgetattr(0);tty.setraw(0)
queries=b"\\x1b]11;?\\x07\\x1b[6n\\x1b[14t"
os.write(1,queries)
received=b"";end=time.monotonic()+1.0
while time.monotonic()<end:
 r,_,_=select.select([0],[],[],max(0,end-time.monotonic()))
 if r: received+=os.read(0,4096)
termios.tcsetattr(0,termios.TCSANOW,saved)
Path({str(report)!r}).write_text(json.dumps({{"queries":queries.hex(),"received":received.hex()}}))
time.sleep(5)
''')
wrapper=base/'shell';wrapper.write_text(f'#!/bin/sh\nexec /usr/bin/python3 {helper}\n');wrapper.chmod(0o755)
(config/'config.lua').write_text(f'return {{shell="{wrapper}",start_hidden=true,hide_on_focus_out=false,animation_ms=0}}')
wayland=os.environ.get('WAYLAND_DISPLAY','wayland-0')
if not os.path.isabs(wayland): wayland=str(Path(os.environ['XDG_RUNTIME_DIR'])/wayland)
env=dict(os.environ,XDG_CONFIG_HOME=str(base/'config'),XDG_RUNTIME_DIR=str(runtime),WAYLAND_DISPLAY=wayland,TAY_SHOT=f'{base}/shot.png,180')
with (base/'log').open('w') as log:
 p=subprocess.Popen([str(root/'target/debug/tayd')],env=env,stdout=log,stderr=log)
 try:p.wait(timeout=15)
 except subprocess.TimeoutExpired:p.terminate();p.wait(timeout=5)
assert report.exists(),(base/'log').read_text()
result=json.loads(report.read_text())
reply=bytes.fromhex(result['received'])
assert b'\x1b[1;1R' in reply, result
assert reply.index(b'\x1b]11;rgb:') < reply.index(b'\x1b[1;1R') < reply.index(b'\x1b[4;'), result
assert b'\x1b]11;rgb:0f0f/1111/1414\x07' in reply, result
import re
assert re.search(rb'\x1b\[4;[1-9][0-9]*;[1-9][0-9]*t', reply), result
print(report.read_text());print(base)
