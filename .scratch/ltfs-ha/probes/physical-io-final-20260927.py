"""专用 RC18：核验 LE 诊断文件，原生格式化后验证最终 I/O 修复及 LE 互读。"""
import hashlib
import json
import os
import re
import shutil
import subprocess
import threading
import xml.etree.ElementTree as ET
from pathlib import Path

b = Path('/root/tape-rs-io-20260927')
assert (b / 'sync-mount-functional.complete').exists()
assert (b / 'le-fm-diagnostic.complete').exists()
assert (b / 'le-fm-immed.confirmed').exists()
assert Path('/sys/module/sg/parameters/allow_dio').read_text().strip() == '0'
assert subprocess.run(['pgrep', '-x', 'ltfs'], capture_output=True).returncode == 1
original = json.loads((b / 'RC0018L9.json').read_text())
previous = json.loads((b / 'le-fm-diagnostic-all.json').read_text())
assert original['volume_uuid'] == previous['volume_uuid']
assert original['barcode'] == previous['barcode'] == 'RC0018L9'
# 格式化前确认基线源内容完整可重建。
for row in original['files']:
    p = b / 'source' / row['path']
    assert p.stat().st_size == row['size']
    h = hashlib.sha256()
    with p.open('rb') as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b''):
            h.update(chunk)
    assert h.hexdigest() == row['sha256'], row['path']
archive = b / 'before-native-format-final'
archive.mkdir()
for name in ['RC0018L9.json', 'sync-position-all.json', 'mount-fast-all.json',
             'le-fm-diagnostic-all.json', 'le-fm-diagnostic.strace']:
    shutil.copy2(b / name, archive / name)

source = (b / 'physical-native-rc18-20260927.py').read_text()
source = source[:source.index("load('RC0018L9')\nbench('write-groups')")]
source = source.replace('native-rc18-measurements.jsonl', 'native-io-final-measurements.jsonl')
source = source.replace('tape-rs-candidate', 'tape-rs-io-final')
source = source.replace("base/'performance_compare'", "base/'performance_compare-io-final'")
source = source.replace("str(base/(loaded+'.json'))", "str(base/'native-final-manifest.json')")
exec(compile(source, 'native-final-helpers', 'exec'))
load('RC0018L9')
new_le_rows = [r for r in previous['files'] if r['group'].startswith('le-fm-diagnostic-')]
assert len(new_le_rows) == 3
with (b / 'le-fm-diagnostic-new.json').open('x') as f:
    json.dump(dict(previous, files=new_le_rows), f)
# 同时核对介质 UUID、驱动序列号和三个新 LE 文件的真实磁带 SHA。
out = subprocess.check_output([
    str(b / 'performance_compare-io-final'), '--physical', '--device', drive,
    '--manifest', str(b / 'le-fm-diagnostic-new.json'), '--source', str(b / 'source'),
    '--group', 'all', '--mode', 'read', '--once'], text=True)
(b / 'native-reads-le-fm.log').write_text(out)
assert any(json.loads(s).get('verified') is True for s in out.splitlines())
assert '驱动器   1 [载带]: RC0018L9' in inventory()

def drive_mode(tag):
    raw = subprocess.check_output(['sg_modes', '--page=0x0f', '--raw', drive])
    assert len(raw) >= 8
    page = 8 + int.from_bytes(raw[6:8], 'big')
    assert len(raw) >= page + 3 and raw[page] & 0x3f == 0x0f
    row = dict(raw_hex=raw.hex(), buffered_mode=(raw[3] >> 4) & 7,
               speed=raw[3] & 15, compression_enabled=bool(raw[page + 2] & 0x80))
    (b / (tag + '.json')).write_text(json.dumps(row))
    return row

before = drive_mode('native-final-mode-before-format')
print('FORMAT RC0018L9 ONLY: native MODE SELECT + FORMAT MEDIUM; old UUID ' + original['volume_uuid'], flush=True)
result = command('mkltfs', '--device', drive, '--volume-id', 'RC0018',
                 '--block-size', '524288', '--compression', '--yes-destroy')
(b / 'native-final-format.log').write_text(result)
uuid_match = re.search(r'Volume UUID = ([0-9a-f-]{36})', result)
assert uuid_match and uuid_match.group(1) != original['volume_uuid']
with (b / 'native-final-manifest.json').open('x') as f:
    json.dump(dict(original, volume_uuid=uuid_match.group(1)), f)
after = drive_mode('native-final-mode-after-format')
assert after['buffered_mode'] == 1 and after['speed'] == 0, after
assert after['compression_enabled'] == before['compression_enabled'], (before, after)

# 只读 procfs 留存实际 fd 预留区，不向计时设备插入诊断 CDB。
seen = threading.Event()
stop = threading.Event()
def observe_reserve():
    while not stop.wait(1):
        text = Path('/proc/scsi/sg/debug').read_text()
        if 'device=sg3 ' in text and 'bufflen=1048576' in text:
            (b / 'native-final-sg-reserve.txt').write_text(text)
            seen.set()
            return
watcher = threading.Thread(target=observe_reserve)
watcher.start()
try:
    bench('write-groups')
finally:
    stop.set()
    watcher.join()
assert seen.is_set(), '未观察到实际1MiB预留缓冲'
unload()
load('RC0018L9')
bench('read')
unload()
emit(dict(kind='complete', passed=True))

helpers = (b / 'physical-rc18-after-position-20260927.py').read_text()
helpers = helpers[:helpers.index("\nassert Path('/sys/module/sg/parameters/allow_dio')")]
exec(compile(helpers, 'le-final-lifecycle', 'exec'))
start_le('le-native-final-start')
admin('tape', 'unassign', barcode)
admin('tape', 'assign', barcode)
source = (b / 'physical-le-rc18-20260927.py').read_text()
source = source[:source.index("show=json.loads(command('tape','show','RC0018L9'))")]
source = source.replace("(base/(b+'.json'))", "(base/'native-final-manifest.json')")
source = source.replace('le-rc18-measurements.jsonl', 'le-native-final.jsonl')
source += "\nload('RC0018L9');reads();unload();emit(dict(kind='complete',passed=True))\n"
with (b / 'le-native-final.py').open('x') as f:
    f.write(source)
subprocess.run(['python3', str(b / 'le-native-final.py')],
               env=dict(os.environ, TAPE_RS_PERF_COMPARE='RC0018L9'), check=True)
label_cache = b / 'le-work/55L3A7802K19LL01/volume_cache/RC0018L9.label.ltfs.0'
assert ET.fromstring(label_cache.read_bytes()).findtext('volumeuuid') == uuid_match.group(1)
stop_le()
assert Path('/sys/module/sg/parameters/allow_dio').read_text().strip() == '0'
(b / 'physical-io-final.complete').write_text('native format + I/O fixes + native/LE SHA passed')
print('FINAL PHYSICAL I/O VALIDATION PASSED', flush=True)
