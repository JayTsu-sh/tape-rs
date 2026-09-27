"""现场 LE 文件标记时序取证：仅 RC18，追加三个独立 4KiB 文件。"""
import hashlib
import os
import signal
import subprocess
import time
from pathlib import Path

b = Path('/root/tape-rs-io-20260927')
assert (b / 'sync-mount-functional.complete').exists()
assert Path('/sys/module/sg/parameters/allow_dio').read_text().strip() == '0'
helpers = (b / 'physical-rc18-after-position-20260927.py').read_text()
helpers = helpers[:helpers.index("\nassert Path('/sys/module/sg/parameters/allow_dio')")]
exec(compile(helpers, 'le-lifecycle-helpers', 'exec'))
assert serial in subprocess.check_output(['sg_inq', '--page=0x80', '/dev/sg3'], text=True)
assert '存储槽   7 [载带]: RC0018L9' in inventory()
start_le('le-fm-diagnostic-start')
admin('tape', 'unassign', barcode)
admin('tape', 'assign', barcode)
source = (b / 'physical-le-rc18-20260927.py').read_text()
source = source[:source.index("show=json.loads(command('tape','show','RC0018L9'))")]
source = source.replace("(base/(b+'.json'))", "(base/'mount-fast-all.json')")
source = source.replace('le-rc18-measurements.jsonl', 'le-fm-diagnostic.jsonl')
os.environ['TAPE_RS_PERF_COMPARE'] = 'RC0018L9'
exec(compile(source, 'le-diagnostic-helpers', 'exec'))
new_rows = []
seed = next(r for r in items[barcode] if r['size'] == 4096)
for i in range(3):
    path = f'comparison/le-fm-diagnostic/{i}.bin'
    local = base / 'source' / path
    local.parent.mkdir(parents=True, exist_ok=True)
    with local.open('xb') as f:
        f.write((base / 'source' / seed['path']).read_bytes())
    new_rows.append(dict(seed, path=path, group=f'le-fm-diagnostic-{i}'))
manifest = json.loads((base / 'mount-fast-all.json').read_text())
manifest['files'] += new_rows
with (base / 'le-fm-diagnostic-all.json').open('x') as f:
    json.dump(manifest, f)
items[barcode] += new_rows
load(barcode)
# 先读取一个原有小文件完成按需挂载，再采样真正的写与普通 sync。
read(seed)
trace = subprocess.Popen(['strace', '-f', '-tt', '-T', '-s', '96', '-e', 'trace=ioctl',
                          '-p', ltfs_pids[0], '-o', str(base / 'le-fm-diagnostic.strace')],
                         stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
try:
    deadline = time.monotonic() + 10
    while True:
        status = dict(x.split(':', 1) for x in (ltfs_proc / 'status').read_text().splitlines() if ':' in x)
        if int(status['TracerPid']) == trace.pid:
            break
        assert int(status['TracerPid']) == 0, '存在其他跟踪器，停止诊断'
        assert trace.poll() is None and time.monotonic() < deadline
        time.sleep(0.1)
    for i in range(3):
        write(f'le-fm-diagnostic-{i}')
        assert trace.poll() is None, 'strace 提前退出，诊断不完整'
finally:
    if trace.poll() is None:
        trace.send_signal(signal.SIGINT)
    _, errors = trace.communicate(timeout=30)
    (base / 'le-fm-diagnostic-strace.log').write_text(errors)
unload()
stop_le()
(base / 'le-fm-diagnostic.complete').write_text('LE 3 ordinary sync traces captured; native cold read pending')
