"""清理版隔离Holo验收：固定SR池、新缓存读写与链接。"""
import hashlib
import mmap
import os
from pathlib import Path
import sys

assert os.environ.get('TAPE_RS_CLEANUP') == 'SR2501L8-SR2502L8'
root = Path('/home/rocky/tape-rs-cleanup-20260927/mnt/sr')
assert os.path.ismount(root.parent)
mode = sys.argv[1]
assert mode in ('create', 'verify', 'clean')
p = root / 'cleanup-20260927'
payload = b'lint cleanup hardware regression\n' * 100
if mode == 'create':
    p.mkdir()
    with (p / 'original').open('xb', buffering=0) as f:
        f.write(payload)
        os.fsync(f.fileno())
    os.setxattr(p / 'original', 'user.cleanup.binary', b'\0\xff')
    (p / 'original').rename(p / 'renamed')
    os.symlink('renamed', p / 'link')
assert not os.path.lexists(p / 'original')
assert os.readlink(p / 'link') == 'renamed'
assert os.getxattr(p / 'renamed', 'user.cleanup.binary') == b'\0\xff'
assert os.getxattr(p / 'renamed', 'user.tape.barcode') == b'SR2502L8'
for name in ('renamed', 'link'):
    with (p / name).open('rb') as f:
        assert f.read() == payload
        with mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as mapping:
            assert mapping[:] == payload
# Existing SR files and symlink metadata remain independently readable.
links = root / 'links'
expected = {'relative': 'target', 'dangling': '../absent & 中文%?#',
            'directory': 'subdir', 'loop': 'loop', '中文': 'target', 'chain': 'relative'}
for name, target in expected.items():
    assert os.path.islink(links / name)
    assert os.readlink(links / name) == target
assert (links / 'chain').read_bytes() == b'created through symlink\n'
assert hashlib.sha256((root / 'seed').read_bytes()).hexdigest() == 'ba3378d7acafca6ec617933b2ea1bdb95bbdb2db4dd4dd6f528ec07cdf827aa7'
if mode == 'clean':
    (p / 'link').unlink()
    (p / 'renamed').unlink()
    p.rmdir()
    assert not os.path.lexists(p)
print('PASS', mode, 'write/fsync/xattr/rename/symlink/read/mmap and original SR files')
