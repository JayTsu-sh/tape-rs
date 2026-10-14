"""普通 sync 实时定位修复验收：追加 3×32 小文件，保留旧数据，不格式化。"""
import hashlib
import json
import os
import shutil
import subprocess
from pathlib import Path

b = Path('/root/tape-rs-io-20260927')
assert (b / 'rc18-functional-performance.complete').exists()
assert Path('/sys/module/sg/parameters/allow_dio').read_text().strip() == '0'
assert subprocess.run(['pgrep', '-x', 'ltfs'], capture_output=True).returncode == 1
original = json.loads((b / 'RC0018L9.json').read_text())
source_rows = [r for r in original['files'] if r['size'] == 4096][:32]
assert len(source_rows) == 32
new_rows = []
for batch in range(3):
    for i, row in enumerate(source_rows):
        dest = f'comparison/sync-position-{batch}/{i:04}.bin'
        p = b / 'source' / dest
        p.parent.mkdir(parents=True, exist_ok=True)
        assert not p.exists()
        shutil.copyfile(b / 'source' / row['path'], p)
        assert hashlib.sha256(p.read_bytes()).hexdigest() == row['sha256']
        new_rows.append(dict(row, path=dest, group=f'sync-position-{batch}'))
for name, rows in [('sync-position-write.json', new_rows),
                   ('sync-position-all.json', original['files'] + new_rows)]:
    with (b / name).open('x') as f:
        json.dump(dict(original, files=rows), f)

# 复用已验证的 serial/inventory/UUID 门控、正常装卸与逐文件 SHA 校验。
source = (b / 'physical-native-rc18-20260927.py').read_text()
source = source[:source.index("load('RC0018L9')\nbench('write-groups')")]
source = source.replace('native-rc18-measurements.jsonl', 'sync-position-measurements.jsonl')
source = source.replace("base/'performance_compare'", "base/'performance_compare-sync-position'")
source = source.replace("str(base/(loaded+'.json'))", "str(base/('sync-position-write.json' if mode == 'write-groups' else 'sync-position-all.json'))")
exec(compile(source, 'native-sync-position-helper', 'exec'))
load('RC0018L9')
for control in ['0', '1']:
    result = subprocess.run(['sg_modes', '--page=0x0f', '--control=' + control,
                             '--hex', '/dev/sg3'], text=True, capture_output=True)
    (b / ('sync-position-mode-' + control + '.log')).write_text(
        result.stdout + result.stderr)
    assert result.returncode == 0
bench('write-groups')
unload()
load('RC0018L9')
bench('read')
unload()
emit(dict(kind='complete', passed=True, scope='native; LE cross-read follows separately'))
(b / 'sync-position-native.complete').write_text('3241 files verified twice after reload')
