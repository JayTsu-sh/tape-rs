"""专用RC18的LE API/CDB取证：不格式化，新增小文件，保持allow_dio=0。"""
import ctypes
import hashlib
import json
import os
import signal
import subprocess
import time
import xml.etree.ElementTree as ET
from pathlib import Path

b = Path('/root/tape-rs-io-20260927')
root = b / 'le-mount'
barcode = 'RC0018L9'
serial = '11EB4A80F1'
expected_uuid = '878f7b34-4910-4d09-9f1f-3e4725ede6d8'
assert os.environ.get('TAPE_RS_LE_API_TRACE') == barcode
assert (b / 'physical-io-final.complete').exists()
assert Path('/sys/module/sg/parameters/allow_dio').read_text().strip() == '0'
assert subprocess.run(['pgrep', '-x', 'ltfs'], capture_output=True).returncode == 1
helpers = (b / 'physical-rc18-after-position-20260927.py').read_text()
helpers = helpers[:helpers.index("\nassert Path('/sys/module/sg/parameters/allow_dio')")]
exec(compile(helpers, 'le-api-lifecycle', 'exec'))
cli = str(b / 'tape-rs-io-final')
assert serial in subprocess.check_output(['sg_inq', '--page=0x80', '/dev/sg3'], text=True)
assert '55L3A7802K19LL01' in subprocess.check_output(['sg_inq', '--page=0x80', '/dev/sg5'], text=True)
inv = inventory()
assert '驱动器   1 [空]' in inv
assert '存储槽   7 [载带]: RC0018L9' in inv and '存储槽   8 [载带]: RC0017L9' in inv
manifest = json.loads((b / 'native-final-manifest.json').read_text())
assert manifest['volume_uuid'] == expected_uuid
seed = next(row for row in manifest['files'] if row['size'] == 4096)
data = (b / 'source' / seed['path']).read_bytes()
assert hashlib.sha256(data).hexdigest() == seed['sha256']
new_rows = []
for name in ['fdatasync', 'fsync', 'ltfs-sync', 'leadm-sync', 'close-only', 'syncfs']:
    for rep in range(2 if name in ('fdatasync', 'fsync', 'ltfs-sync', 'leadm-sync') else 1):
        row = dict(seed, path=f'comparison/le-api-20260927/{name}-{rep}.bin', group='le-api')
        local = b / 'source' / row['path']
        local.parent.mkdir(parents=True, exist_ok=True)
        with local.open('xb') as f:
            f.write(data)
        new_rows.append(row)
with (b / 'le-api-new.json').open('x') as f:
    json.dump(dict(manifest, files=new_rows), f)
with (b / 'le-api-all.json').open('x') as f:
    json.dump(dict(manifest, files=manifest['files'] + new_rows), f)
log = (b / 'le-api-events.jsonl').open('x')


def event(name, action, **extra):
    start = time.time_ns()
    log.write(json.dumps(dict(phase=name, event='begin', time_ns=start, **extra)) + '\n')
    log.flush()
    try:
        result = action()
    except BaseException as e:
        log.write(json.dumps(dict(phase=name, event='error', time_ns=time.time_ns(), error=repr(e))) + '\n')
        log.flush()
        raise
    end = time.time_ns()
    row = dict(phase=name, event='end', time_ns=end, seconds=(end-start)/1e9, **extra)
    log.write(json.dumps(row) + '\n')
    log.flush()
    print(json.dumps(row), flush=True)
    time.sleep(0.3)  # 独立观察窗口，不计入调用耗时；异步命令在事件间隙单列。
    return result


libc = ctypes.CDLL(None, use_errno=True)
libc.syncfs.argtypes = [ctypes.c_int]
libc.syncfs.restype = ctypes.c_int


def syncfs(fd):
    if libc.syncfs(fd) != 0:
        err = ctypes.get_errno()
        raise OSError(err, os.strerror(err))


def read_and_check(path, expected):
    content = path.read_bytes()
    assert hashlib.sha256(content).hexdigest() == expected


start_le('le-api-start')
admin('tape', 'unassign', barcode)
admin('tape', 'assign', barcode)
pids = subprocess.check_output(['pgrep', '-x', 'ltfs'], text=True).split()
assert len(pids) == 1
proc = Path('/proc') / pids[0]
trace = subprocess.Popen([
    'strace', '-f', '-ttt', '-T', '-xx', '-yy', '-s', '96', '-e', 'trace=ioctl',
    '-p', pids[0], '-o', str(b / 'le-api-scsi.strace'),
], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
try:
    deadline = time.monotonic() + 10
    while True:
        status = dict(x.split(':', 1) for x in (proc / 'status').read_text().splitlines() if ':' in x)
        if int(status['TracerPid']) == trace.pid:
            break
        assert int(status['TracerPid']) == 0 and trace.poll() is None and time.monotonic() < deadline
        time.sleep(0.1)
    event('leadm-move-to-drive', lambda: admin('tape', 'move', '-L', 'drive', '-d', serial, barcode))
    existing = root / barcode / seed['path']
    fd = event('open-existing-first', lambda: os.open(existing, os.O_RDONLY), path=seed['path'])
    try:
        first = event('read-existing-first', lambda: os.read(fd, 4096))
        assert hashlib.sha256(first).hexdigest() == seed['sha256']
    finally:
        event('close-existing-first', lambda: os.close(fd))
    label = b / 'le-work/55L3A7802K19LL01/volume_cache/RC0018L9.label.ltfs.0'
    assert ET.fromstring(label.read_bytes()).findtext('volumeuuid') == expected_uuid
    fd = event('open-existing-warm', lambda: os.open(existing, os.O_RDONLY))
    try:
        event('fsync-existing-clean', lambda: os.fsync(fd))
        event('fdatasync-existing-clean', lambda: os.fdatasync(fd))
    finally:
        event('close-existing-warm', lambda: os.close(fd))
    target_dir = root / barcode / 'comparison/le-api-20260927'
    event('create-test-directory', lambda: target_dir.mkdir())
    event('ltfs-sync-baseline', lambda: os.setxattr(root / barcode, 'user.ltfs.sync', b'1'))
    for row in new_rows:
        name = Path(row['path']).stem
        target = root / barcode / row['path']
        fd = event(name + ':open-create', lambda: os.open(target, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600))
        try:
            count = event(name + ':write', lambda: os.write(fd, data), bytes=len(data))
            assert count == len(data)
            if name.startswith('fdatasync'):
                event(name + ':fdatasync', lambda: os.fdatasync(fd))
                event(name + ':fdatasync-clean', lambda: os.fdatasync(fd))
            elif name.startswith('fsync'):
                event(name + ':fsync', lambda: os.fsync(fd))
                event(name + ':fsync-clean', lambda: os.fsync(fd))
            elif name.startswith('ltfs-sync'):
                event(name + ':ltfs-sync', lambda: os.setxattr(root / barcode, 'user.ltfs.sync', b'1'))
                event(name + ':ltfs-sync-clean', lambda: os.setxattr(root / barcode, 'user.ltfs.sync', b'1'))
            elif name.startswith('leadm-sync'):
                event(name + ':leadm-sync', lambda: admin('tape', 'sync', barcode))
                event(name + ':leadm-sync-clean', lambda: admin('tape', 'sync', barcode))
            elif name.startswith('syncfs'):
                event(name + ':syncfs', lambda: syncfs(fd))
        finally:
            event(name + ':close', lambda: os.close(fd))
        # 观察单个API之后仍然欠缺哪些索引/设备动作，并为下个样例建立相同起点。
        event(name + ':settle-ltfs-sync', lambda: os.setxattr(root / barcode, 'user.ltfs.sync', b'1'))
        assert trace.poll() is None
    # 仅metadata变化，区分POSIX两种接口与安装版的真实实现。
    target = root / barcode / new_rows[0]['path']
    fd = event('metadata:open', lambda: os.open(target, os.O_RDWR))
    try:
        event('metadata:chmod', lambda: os.fchmod(fd, 0o640))
        event('metadata:fdatasync', lambda: os.fdatasync(fd))
        event('metadata:fsync', lambda: os.fsync(fd))
    finally:
        event('metadata:close', lambda: os.close(fd))
    event('metadata:ltfs-sync', lambda: os.setxattr(root / barcode, 'user.ltfs.sync', b'1'))
    event('leadm-unload-dirty-ip', lambda: admin('tape', 'move', '-L', 'homeslot', barcode))
    event('leadm-reload', lambda: admin('tape', 'move', '-L', 'drive', '-d', serial, barcode))
    event('open-after-reload-and-check', lambda: read_and_check(root / barcode / new_rows[0]['path'], seed['sha256']))
    event('leadm-unload-clean', lambda: admin('tape', 'move', '-L', 'homeslot', barcode))
    label = b / 'le-work/55L3A7802K19LL01/volume_cache/RC0018L9.label.ltfs.0'
    assert ET.fromstring(label.read_bytes()).findtext('volumeuuid') == expected_uuid
    event('le-unmount-and-exit', stop_le)
finally:
    if trace.poll() is None:
        trace.send_signal(signal.SIGINT)
    _, errors = trace.communicate(timeout=30)
    (b / 'le-api-strace-stderr.log').write_text(errors)
assert Path('/sys/module/sg/parameters/allow_dio').read_text().strip() == '0'
assert not os.path.ismount(root)
assert '驱动器   1 [空]' in inventory()
# 冷挂载后用直接Rust接口校验新增文件，避免将LE缓存读误当作磁带持久化验证。
print(native('load', '--device', '/dev/sg5', '--drive', '0', '--slot', '7'), flush=True)
print(native('drive-load', '--device', '/dev/sg3'), flush=True)
args = [str(b / 'performance_compare-io-final'), '--physical', '--device', '/dev/sg3',
        '--manifest', str(b / 'le-api-new.json'), '--source', str(b / 'source'),
        '--group', 'all', '--mode', 'read', '--once']
checked = subprocess.check_output(args, text=True, env=dict(os.environ, TAPE_RS_PERF_COMPARE='RC0018L9-RC0017L9'))
(b / 'le-api-native-verify.log').write_text(checked)
assert any(json.loads(line).get('verified') is True for line in checked.splitlines())
print(native('drive-unload', '--device', '/dev/sg3'), flush=True)
print(native('unload', '--device', '/dev/sg5', '--drive', '0', '--slot', '7'), flush=True)
assert '持有者:     (无)' in native('pr-status', '--device', '/dev/sg3')
(b / 'le-api-trace.complete').write_text('LE APIs traced; native cold SHA of all new files passed; RC18 home; direct IO disabled')
print('LE API TRACE COMPLETE', flush=True)
