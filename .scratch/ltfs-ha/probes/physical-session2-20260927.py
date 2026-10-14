"""RC18专用：三节点同机隔离集群验收常驻卷；无格式化，allow_dio保持0。"""
import hashlib
import http.client
import json
import os
import signal
import subprocess
import time
from pathlib import Path

b = Path('/root/tape-rs-io-20260927')
assert os.environ.get('TAPE_RS_SESSION_TRACE') == 'RC0018L9'
assert (b / 'session-native.complete').exists()
assert subprocess.run(['pgrep', '-f', str(b / 'ltfsd-')], capture_output=True).returncode == 1
assert Path('/sys/module/sg/parameters/allow_dio').read_text().strip() == '0'
for process in ('ltfs', 'ltfsd'):
    assert subprocess.run(['pgrep', '-x', process], capture_output=True).returncode == 1
cli = str(b / 'tape-rs-io-final')
assert '11EB4A80F1' in subprocess.check_output(['sg_inq', '--page=0x80', '/dev/sg3'], text=True)
assert '55L3A7802K19LL01' in subprocess.check_output(['sg_inq', '--page=0x80', '/dev/sg5'], text=True)
inv = subprocess.check_output([cli, 'inventory', '--device', '/dev/sg5', '--no-drive-scan'], text=True)
assert '驱动器   1 [空]' in inv and '存储槽   7 [载带]: RC0018L9' in inv and '存储槽   8 [载带]: RC0017L9' in inv
manifest = json.loads((b / 'session-all.json').read_text())
assert manifest['volume_uuid'] == '878f7b34-4910-4d09-9f1f-3e4725ede6d8'
run = b / 'session-cluster'
assert run.is_dir()  # 复用已经提交的池归属与Raft状态，不重建新池
rows = []
for i in range(8):
    data = hashlib.sha256(f'session2-{i}'.encode()).digest() * 128
    path = f'comparison/session2-20260927/f{i}.bin'
    source = b / 'source' / path
    source.parent.mkdir(parents=True, exist_ok=True)
    with source.open('xb') as f:
        f.write(data)
    rows.append(dict(path=path, size=len(data), sha256=hashlib.sha256(data).hexdigest(), group='session'))
(b / 'session2-new.json').write_text(json.dumps(dict(manifest, files=rows)))
(b / 'session2-all.json').write_text(json.dumps(dict(manifest, files=manifest['files'] + rows)))
events = (b / 'session2-events.jsonl').open('x')


def emit(**row):
    row['time_ns'] = time.time_ns()
    events.write(json.dumps(row) + '\n')
    events.flush()
    print(json.dumps(row), flush=True)


def request(node, method, path, body=None):
    conn = http.client.HTTPConnection('127.0.0.1', 17701 + (node - 1) * 10, timeout=900)
    try:
        conn.request(method, path, body=body)
        reply = conn.getresponse()
        return reply.status, reply.read()
    finally:
        conn.close()


nodes = []
for i in range(1, 4):
    args = [str(b / 'ltfsd-perf-final'), '--id', str(i), '--listen', f'127.0.0.1:{17700+(i-1)*10}',
            '--data-dir', str(run / f'node{i}'), '--changer-serial', '55L3A7802K19LL01',
            '--drive-serial', '11EB4A80F1', '--client-listen', f'127.0.0.1:{17701+(i-1)*10}',
            '--client-port', str(17701+(i-1)*10), '--batch-files', '1', '--shutdown-grace-s', '900']
    for peer in range(1, 4):
        args.extend(['--peer', f'{peer}=127.0.0.1:{17700+(peer-1)*10}'])
    log = (b / f'session2-node{i}.log').open('x')
    p = subprocess.Popen(args, stdout=log, stderr=subprocess.STDOUT,
                         env=dict(os.environ, RUST_LOG='info,tape_rs::scsi::device=debug'), start_new_session=True)
    log.close()
    nodes.append(p)
(b / 'session2-nodes.json').write_text(json.dumps([p.pid for p in nodes]))
deadline = time.monotonic() + 180
while True:
    assert all(p.poll() is None for p in nodes)
    assert time.monotonic() < deadline
    leader = None
    for node in range(1, 4):
        try:
            status, raw = request(node, 'GET', '/cluster')
            state = json.loads(raw)
            if status == 200 and state.get('leader') == node and state.get('executor', {}).get('fenced'):
                leader = node
                break
        except (OSError, ValueError, AttributeError):
            pass
    if leader:
        break
    time.sleep(1)
emit(phase='leader', node=leader)
status, raw = request(leader, 'GET', '/admin/pools')
assert status == 200 and any(p['uuid'] == '98310642-518f-4db9-872a-3ba69933011e' and 'RC0018L9' in p['tapes'] for p in json.loads(raw)['pools'])
deadline = time.monotonic() + 900
while True:
    status, raw = request(leader, 'GET', '/cluster')
    state = json.loads(raw)
    assert state.get('leader') == leader and time.monotonic() < deadline, state
    if state.get('serving_round'):
        break
    time.sleep(2)
emit(phase='serving', state=state)
for i, row in enumerate(rows):
    data = (b / 'source' / row['path']).read_bytes()
    start = time.monotonic()
    emit(phase='put-begin', path=row['path'])
    status, raw = request(leader, 'PUT', '/files/' + row['path'] + '?wait=1', data)
    emit(phase='put', path=row['path'], seconds=time.monotonic()-start, status=status, response=raw.decode())
    assert status == 201
    if i in (2, 7):
        start = time.monotonic()
        status, content = request(leader, 'GET', '/files/' + rows[0]['path'])
        assert status == 200 and hashlib.sha256(content).hexdigest() == rows[0]['sha256']
        emit(phase='read-between-batches', seconds=time.monotonic()-start, verified=True)
# 同时发停机信号，避免主动制造接管；保留全部日志，失败不自动强杀。
emit(phase='shutdown-begin')
for p in nodes:
    p.send_signal(signal.SIGTERM)
for p in nodes:
    assert p.wait(timeout=1000) == 0
assert subprocess.run(['pgrep', '-x', 'ltfsd'], capture_output=True).returncode == 1
assert '持有者:     (无)' in subprocess.check_output([cli, 'pr-status', '--device', '/dev/sg3'], text=True)
emit(phase='shutdown-complete')
# 计划停机将介质unthread，但仍留drive；冷load再用原生接口校验。
subprocess.run([cli, 'drive-load', '--device', '/dev/sg3'], check=True)
args = [str(b / 'performance_compare-perf-final'), '--physical', '--device', '/dev/sg3',
        '--manifest', str(b / 'session2-all.json'), '--source', str(b / 'source'), '--group', 'all', '--mode', 'read', '--once']
checked = subprocess.check_output(args, text=True, env=dict(os.environ, TAPE_RS_PERF_COMPARE='RC0018L9-RC0017L9'))
(b / 'session2-native-verify.log').write_text(checked)
assert any(json.loads(x).get('verified') is True for x in checked.splitlines())
emit(phase='native-cold-read', verified=True)
subprocess.run([cli, 'drive-unload', '--device', '/dev/sg3'], check=True)
subprocess.run([cli, 'unload', '--device', '/dev/sg5', '--drive', '0', '--slot', '7'], check=True)
# 互读由后续独立步骤完成；此标志只代表daemon+Rust验收。
assert Path('/sys/module/sg/parameters/allow_dio').read_text().strip() == '0'
(b / 'session2-native.complete').write_text('8 separate commits, mixed reads, shutdown, native cold SHA passed; RC18 home')
emit(phase='complete')

helpers = (b / 'physical-rc18-after-position-20260927.py').read_text()
helpers = helpers[:helpers.index("\nassert Path('/sys/module/sg/parameters/allow_dio')")]
exec(compile(helpers, 'le-session-lifecycle', 'exec'))
cli = str(b / 'tape-rs-io-final')
start_le('session2-le-start')
admin('tape', 'unassign', barcode)
admin('tape', 'assign', barcode)
admin('tape', 'move', '-L', 'drive', '-d', serial, barcode)
start = time.monotonic()
all_rows = json.loads((b / 'session2-all.json').read_text())['files']
for row in all_rows:
    h = hashlib.sha256()
    with (b / 'le-mount' / barcode / row['path']).open('rb') as f:
        while chunk := f.read(1024 * 1024):
            h.update(chunk)
    assert h.hexdigest() == row['sha256'], row['path']
emit(phase='le-all-files-read', count=len(all_rows), seconds=time.monotonic()-start, verified=True)
label = b / 'le-work/55L3A7802K19LL01/volume_cache/RC0018L9.label.ltfs.0'
assert ET.fromstring(label.read_bytes()).findtext('volumeuuid') == '878f7b34-4910-4d09-9f1f-3e4725ede6d8'
admin('tape', 'move', '-L', 'homeslot', barcode)
stop_le()
assert '驱动器   1 [空]' in inventory()
assert '存储槽   7 [载带]: RC0018L9' in inventory()
assert Path('/sys/module/sg/parameters/allow_dio').read_text().strip() == '0'
(b / 'session2-interop.complete').write_text('candidate daemon + native cold SHA + LE all-file SHA passed; RC18 home; direct0')
emit(phase='interop-complete')
