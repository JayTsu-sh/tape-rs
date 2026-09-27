"""专用 SR 池：源 mkltfs 完成但回收完成记录尚未提交时终止执行者。"""
import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import time
import urllib.request

BASE = Path('/home/rocky/tape-rs-reclaim-formatted-20260927')
assert os.environ.get('TAPE_RS_RECLAIM_FORMATTED') == 'SR2501L8-to-SR2502L8'
pids = subprocess.check_output(['pgrep', '-x', 'ltfsd'], text=True).split()
assert len(pids) == 1
pid = int(pids[0])
assert os.readlink(f'/proc/{pid}/exe') == str(BASE / 'ltfsd')
args = Path(f'/proc/{pid}/cmdline').read_bytes().split(bytes([0]))
assert str(BASE / 'test-data').encode() in args
status = json.loads((BASE / 'test-data/status.json').read_text())
with urllib.request.urlopen('http://127.0.0.1:7501/cluster', timeout=5) as r:
    serving = json.load(r)
assert status['role'] == 'Leader' and serving['serving_round'] == status['executor']['round']
assert len(status['pools']) == 1
assert set(status['pools'][0]['tapes']) == {'SR2501L8', 'SR2502L8'}
assert status['pools'][0]['uuid'] == '51cfd06c-bf79-4bbf-ac85-886608a0ab4b'
tids = [int(p.name) for p in Path(f'/proc/{pid}/task').iterdir()
        if (p / 'comm').read_text().strip() == 'ltfsd-exec']
assert len(tids) == 1
assert not (BASE / 'cut.ready').exists()
log_offset = (BASE / 'ltfsd.log').stat().st_size
trace = BASE / 'cut.trace'
with (BASE / 'cut-strace.log').open('w') as log:
    tracer = subprocess.Popen([
        str(BASE / 'strace'), '-p', str(tids[0]), '-e', 'trace=ioctl',
        '-e', 'inject=ioctl:delay_enter=500ms', '-v', '-xx', '-s', '96',
        '-tt', '-T', '-o', str(trace)], stdout=log, stderr=log)
    try:
        deadline = time.monotonic() + 10
        while f'TracerPid:\t{tracer.pid}' not in Path(f'/proc/{pid}/task/{tids[0]}/status').read_text():
            assert time.monotonic() < deadline
            time.sleep(.02)
        (BASE / 'cut.ready').write_text(json.dumps({'pid': pid, 'tid': tids[0], 'round': status['executor']['round']}))
        deadline = time.monotonic() + 300
        while time.monotonic() < deadline:
            with (BASE / 'ltfsd.log').open('rb') as f:
                f.seek(log_offset)
                lines = f.read().decode().splitlines()
            starts = [s for s in lines if 'mkltfs 开始: volume_id=SR2501 ' in s]
            marker = []
            if starts:
                assert len(starts) == 1, 'unexpected repeated source format'
                volume_uuid = starts[0].split('uuid=', 1)[1].strip()
                marker = [s for s in lines if 'mkltfs 完成: ' + volume_uuid in s]
            if marker:
                before = trace.read_text()
                formats = [s for s in before.splitlines() if 'cmdp="\\x04' in s
                           and ') = 0' in s and 'status=0,' in s]
                assert len(formats) == 1, 'successful source FORMAT not proven; no kill'
                current = json.loads((BASE / 'test-data/status.json').read_text())
                source_state = next(t for t in current['tapes'] if t['barcode'] == 'SR2501L8')
                assert source_state['state'] == 'reclaiming'
                assert current['last_reclaim'] is None
                os.kill(pid, signal.SIGKILL)
                tracer.wait(timeout=10)
                after = trace.read_text()
                trace_lines = after.splitlines()
                format_end = next(i for i, s in enumerate(trace_lines)
                                  if 'cmdp="\\x04' in s and ') = 0' in s and 'status=0,' in s)
                assert not any('cmdp="\\x08' in s and ') = 0' in s
                               for s in trace_lines[format_end + 1:]), 'remount READ completed'
                final_log = (BASE / 'ltfsd.log').read_bytes()[log_offset:].decode()
                assert '回收完毕' not in final_log
                db = sqlite3.connect('file:' + str(BASE / 'test-data/directory.db') + '?mode=ro', uri=True)
                db.row_factory = sqlite3.Row
                rows = [dict(r) for r in db.execute('select * from files order by path')]
                # All current paths, including tombstones, must have moved.
                assert rows and {r['barcode'] for r in rows} == {'SR2502L8'}
                event = {'pid': pid, 'round': status['executor']['round'], 'marker': marker, 'formatted_uuid': volume_uuid, 'state_at_kill': current,
                         'files': rows, 'trace_at_kill': before, 'trace_after_exit': after}
                (BASE / 'cut.event.json').write_text(json.dumps(event, ensure_ascii=False, indent=2))
                print('PASS source mkltfs complete; killed with reclaiming state before completion record', flush=True)
                break
            time.sleep(.01)
        else:
            raise TimeoutError('completed mkltfs not observed; no kill performed')
    finally:
        if tracer.poll() is None:
            tracer.terminate()
        tracer.wait(timeout=10)
