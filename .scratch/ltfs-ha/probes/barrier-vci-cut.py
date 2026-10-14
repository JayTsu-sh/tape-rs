"""隔离 RT2502L8：只在已观测的增量索引窗口终止测试 Leader。"""
import json
import os
import pathlib
import re
import signal
import sqlite3
import subprocess
import sys
import time

base = pathlib.Path('/home/rocky/tape-rs-barrier-vci-20260925')
assert os.environ.get('TAPE_RS_XATTR_CUT') == 'RT2502L8'
mode = sys.argv[1]
assert mode in ('barrier', 'vci')
pids = subprocess.check_output(['pgrep', '-x', 'ltfsd'], text=True).split()
assert len(pids) == 1
pid = int(pids[0])
assert os.readlink(f'/proc/{pid}/exe') == str(base / 'ltfsd')
s = json.loads((base / 'test-data/status.json').read_text())
assert s['role'] == 'Leader' and 'RT2502L8' in s['local']
assert s['pools'][0]['tapes'] == ['RT2502L8']
tids = [int(p.name) for p in pathlib.Path(f'/proc/{pid}/task').iterdir()
        if (p / 'comm').read_text().strip() == 'ltfsd-exec']
assert len(tids) == 1
trace = base / (mode + '.trace')
assert not trace.exists()
log_start = len((base / 'test.log').read_text())
log = open(base / (mode + '-strace.log'), 'w')
tracer = subprocess.Popen([str(base / 'strace'), '-p', str(tids[0]), '-e', 'trace=ioctl',
    '-e', 'inject=ioctl:delay_enter=200ms', '-v', '-xx', '-s', '96', '-tt', '-T',
    '-o', str(trace)], stdout=log, stderr=log)

def completed(lines):
    out = []
    for line in lines:
        if ') = 0' not in line or 'status=0,' not in line:
            continue
        match = re.search(r'cmdp="([^"]+)"', line)
        if match:
            cdb = bytes(int(x, 16) for x in re.findall(r'\\x([0-9a-f]{2})', match[1]))
            out.append((cdb, line))
    return out

try:
    deadline = time.monotonic() + 10
    while f'TracerPid:\t{tracer.pid}' not in pathlib.Path(f'/proc/{pid}/task/{tids[0]}/status').read_text():
        assert time.monotonic() < deadline
        time.sleep(.05)
    (base / (mode + '.ready')).write_text(json.dumps({'pid': pid, 'round': s['executor']['round']}))
    deadline = time.monotonic() + 180
    while time.monotonic() < deadline:
        lines = trace.read_text().splitlines() if trace.exists() else []
        ops = completed(lines)
        writes = [i for i, (cdb, _) in enumerate(ops) if cdb[0] == 0x0a]
        if writes:
            assert len(writes) == 1, '索引写次数不符合预期，未执行终止'
            i = writes[0]
            marker = ''.join('\\x%02x' % b for b in b'<ltfsincrementalindex')
            assert marker in ops[i][1], '不是增量索引，未执行终止'
            following = ops[i + 1:]
            closing = [(c, l) for c, l in following if c == bytes([0x10, 0, 0, 0, 1, 0])]
            barriers = [(c, l) for c, l in following if c == bytes([0x10, 0, 0, 0, 0, 0])]
            vcis = [(c, l) for c, l in following if c[0] == 0x8d]
            hit = bool(closing) if mode == 'barrier' else bool(vcis)
            if hit:
                assert len(closing) == 1
                if mode == 'barrier':
                    assert not barriers and not vcis, '已越过barrier窗口'
                else:
                    assert len(barriers) == 1 and len(vcis) == 1 and vcis[0][0][7] == 0
                    assert '\\x08\\x0c' in vcis[0][1], '不是VCI属性'
                recent = (base / 'test.log').read_text()[log_start:]
                assert 'commit: gen ' not in recent and '执行线程: 已提交' not in recent
                db = sqlite3.connect('file:' + str(base / 'test-data/directory.db') + '?mode=ro', uri=True)
                rows = db.execute("select * from files where path='/rc/xattr-fault-20260925'").fetchall()
                assert len(rows) == 1
                os.kill(pid, signal.SIGKILL)
                (base / (mode + '.event')).write_text(json.dumps(dict(mode=mode, pid=pid,
                    round=s['executor']['round'], catalog_before_kill=rows, lines_at_kill=lines), indent=2))
                print('PASS:', mode, '窗口已命中并终止测试进程', flush=True)
                break
        time.sleep(.01)
    else:
        raise TimeoutError('未观测到指定窗口，未执行终止')
finally:
    if tracer.poll() is None:
        tracer.terminate()
    tracer.wait(timeout=10)
