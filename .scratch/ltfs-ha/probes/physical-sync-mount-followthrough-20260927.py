"""等待 sync 验证成功，再验证正常挂载快速路径和 LE 全量交叉读取。"""
import os
import subprocess
import time
from pathlib import Path

b = Path('/root/tape-rs-io-20260927')
deadline = time.monotonic() + 3600
pid = int((b / 'sync-position.pid').read_text())
while not (b / 'sync-position-native.complete').exists():
    assert Path(f'/proc/{pid}').exists(), 'sync 验证提前退出；停止后续操作'
    assert time.monotonic() < deadline, '等待 sync 验证超时；停止后续操作'
    time.sleep(5)
print('SYNC POSITION NATIVE PASSED', flush=True)
subprocess.run(['python3', str(b / 'physical-mount-fast-20260927.py')],
               env=dict(os.environ, TAPE_RS_PERF_COMPARE='RC0018L9-RC0017L9'), check=True)
assert (b / 'mount-fast-native.complete').exists()

helpers = (b / 'physical-rc18-after-position-20260927.py').read_text()
helpers = helpers[:helpers.index("\nassert Path('/sys/module/sg/parameters/allow_dio')")]
exec(compile(helpers, 'le-lifecycle-helpers', 'exec'))
start_le('le-sync-mount-cross-start')
admin('tape', 'unassign', barcode)
admin('tape', 'assign', barcode)
source = (b / 'physical-le-rc18-20260927.py').read_text()
source = source[:source.index("show=json.loads(command('tape','show','RC0018L9'))")]
source = source.replace("(base/(b+'.json'))", "(base/'mount-fast-all.json')")
source = source.replace('le-rc18-measurements.jsonl', 'le-sync-mount-cross.jsonl')
source += "\nload('RC0018L9');reads();unload();emit(dict(kind='complete',passed=True))\n"
cross = b / 'le-sync-mount-cross.py'
with cross.open('x') as f:
    f.write(source)
subprocess.run(['python3', str(cross)],
               env=dict(os.environ, TAPE_RS_PERF_COMPARE='RC0018L9'), check=True)
stop_le()
assert Path('/sys/module/sg/parameters/allow_dio').read_text().strip() == '0'
(b / 'sync-mount-functional.complete').write_text('3273 files: sync + fast mount + native and LE SHA passed')
print('SYNC/MOUNT ALL PASSED', flush=True)
