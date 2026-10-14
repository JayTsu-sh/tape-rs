"""RC18 LE成功后交叉读取、重建空卷、Rust对照及LE交叉读取；失败即停。"""
import json
import os
import shutil
import subprocess
import time
import xml.etree.ElementTree as ET
from pathlib import Path

b = Path('/root/tape-rs-io-20260927')
cli = str(b / 'tape-rs-candidate')
lead = '/opt/ibm/ltfsle/bin/leadm'
serial = '11EB4A80F1'
barcode = 'RC0018L9'
env = dict(os.environ, TAPE_RS_PERF_COMPARE='RC0018L9-RC0017L9')


def native(*args):
    return subprocess.check_output([cli, *args], text=True)


def admin(*args):
    return subprocess.check_output([lead, *args, '-s', 'localhost:17600'], text=True)


def inventory():
    return native('inventory', '--device', '/dev/sg5', '--no-drive-scan')


def stop_le():
    subprocess.run(['umount', str(b / 'le-mount')], check=True)
    deadline = time.monotonic() + 1800
    while subprocess.run(['pgrep', '-x', 'ltfs'], capture_output=True).returncode == 0:
        assert time.monotonic() < deadline, 'LE shutdown did not complete'
        time.sleep(1)
    pr = native('pr-status', '--device', '/dev/sg3')
    assert '持有者:     (无)' in pr, pr


def start_le(tag):
    assert subprocess.run(['pgrep', '-x', 'ltfs'], capture_output=True).returncode == 1
    with (b / (tag + '.log')).open('x') as log:
        p = subprocess.Popen([
            '/opt/ibm/ltfsle/bin/ltfs', str(b / 'le-mount'), '-f',
            '-o', 'changer_devname=/dev/sg5',
            '-o', 'work_directory=' + str(b / 'le-work'),
            '-o', 'admin_port=17600', '-o', 'sync_type=unmount',
            '-o', 'volser_only_folder',
        ], stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
    (b / 'le.pid').write_text(str(p.pid))
    deadline = time.monotonic() + 120
    while not os.path.ismount(b / 'le-mount'):
        assert p.poll() is None and time.monotonic() < deadline
        time.sleep(1)


assert Path('/sys/module/sg/parameters/allow_dio').read_text().strip() == '0'
assert subprocess.run(['pgrep', '-x', 'ltfs'], capture_output=True).returncode == 1
assert serial in subprocess.check_output(['sg_inq', '--page=0x80', '/dev/sg3'], text=True)
assert '55L3A7802K19LL01' in subprocess.check_output(['sg_inq', '--page=0x80', '/dev/sg5'], text=True)
assert '驱动器   1 [载带]: RC0018L9' in inventory()
assert (b / 'native-before-position-fix/RC0018L9.json').exists()
assert (b / 'source/comparison/growth').is_dir()
print(native('drive-unload', '--device', '/dev/sg3'), flush=True)
print(native('unload', '--device', '/dev/sg5', '--drive', '0', '--slot', '7'), flush=True)
start_le('le-rc18-reset-after-position')
show = json.loads(admin('tape', 'show', barcode))
assert show['slot_type'] == 'SLOT' and show['home_slot'] == 1007, show
assert json.loads(admin('tape', 'show', barcode))['slot_type'] == 'SLOT'
print('FORMAT RC0018L9 ONLY: common empty baseline', flush=True)
print(admin('tape', 'format', '--force', '--drive-serial', serial,
            '--volume-name', 'tape-rs-benchmark', barcode), flush=True)
label = b / 'le-work/55L3A7802K19LL01/volume_cache/RC0018L9.label.ltfs.0'
root = ET.fromstring(label.read_bytes())
assert root.findtext('blocksize') == '524288'
manifest = b / 'RC0018L9.json'
data = json.loads(manifest.read_text())
data['volume_uuid'] = root.findtext('volumeuuid')
assert data['volume_uuid']
manifest.write_text(json.dumps(data))
print(admin('tape', 'move', '-L', 'homeslot', barcode), flush=True)
stop_le()
assert Path('/sys/module/sg/parameters/allow_dio').read_text().strip() == '0'
print('NATIVE PERFORMANCE', flush=True)
subprocess.run(['python3', str(b / 'physical-native-rc18-20260927.py')], env=env, check=True)

# LE 重新分配清掉旧卷命名空间，从 Rust 写入的卷校验所有文件。
print('CROSS READ: LE reads Rust data', flush=True)
start_le('le-rc18-native-read-start')
admin('tape', 'unassign', barcode)
admin('tape', 'assign', barcode)
source = (b / 'physical-le-rc18-20260927.py').read_text()
source = source[:source.index("show=json.loads(command('tape','show','RC0018L9'))")]
source = source.replace('le-rc18-measurements.jsonl', 'le-reads-native-rc18.jsonl')
source += "\nload('RC0018L9');reads();unload();emit(dict(kind='complete',passed=True))\n"
cross = b / 'le-reads-native-rc18.py'
cross.write_text(source)
subprocess.run(['python3', str(cross)], env=dict(os.environ, TAPE_RS_PERF_COMPARE='RC0018L9'), check=True)
stop_le()
assert Path('/sys/module/sg/parameters/allow_dio').read_text().strip() == '0'
(b / 'rc18-functional-performance.complete').write_text('LE/native full SHA, reloads and both cross reads passed')
print('ALL RC18 FUNCTIONAL/PERFORMANCE CHECKS PASSED', flush=True)
