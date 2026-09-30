"""等待第一候选正常归槽后运行最终候选；前序失败不接管硬件。"""
import hashlib
import os
import subprocess
import time
from pathlib import Path
b = Path('/root/tape-rs-io-20260927')
assert os.environ.get('TAPE_RS_SESSION_TRACE') == 'RC0018L9'
deadline = time.monotonic() + 2400
while not (b / 'session-native.complete').exists():
    assert time.monotonic() < deadline
    pid = int((b / 'session-supervisor.pid').read_text())
    os.kill(pid, 0)
    time.sleep(5)
for name, digest in [('ltfsd-perf-final', 'a6a17e27991afd95f577f0a296bf53b24047223079a99167e2515587aff83853'),
                     ('performance_compare-perf-final', '317b09ce195156163641dfa0699a4714881b1dd8fce5619740a3ea26d0824738')]:
    assert hashlib.sha256((b / name).read_bytes()).hexdigest() == digest
with (b / 'session2-run.log').open('x') as log:
    p = subprocess.Popen(['python3', str(b / 'physical-session2-20260927.py')], stdout=log, stderr=subprocess.STDOUT)
(b / 'session2-supervisor.pid').write_text(str(p.pid))
print('FINAL SESSION STARTED', p.pid, flush=True)
assert p.wait() == 0
assert (b / 'session2-interop.complete').exists()
print('FINAL SESSION COMPLETE', flush=True)
