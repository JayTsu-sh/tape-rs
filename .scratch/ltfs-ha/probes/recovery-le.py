"""故障恢复后 Full 索引的 LE/FUSE 往返，显式副本与挂载门控。"""
import errno
import hashlib
import json
import mmap
import os
from pathlib import Path
import sys

assert os.environ.get('TAPE_RS_RECOVERY_LE') == 'RT2502L8-TS1000L8'
mode, manifest_path = sys.argv[1:]
assert mode in ('le', 'fuse', 'fuse-regular')
root = Path('/ltfs/TS1000L8' if mode == 'le' else '/home/rocky/tape-rs-recovery-le-20260925/mnt')
assert os.path.ismount('/ltfs' if mode == 'le' else root)
manifest = json.loads(Path(manifest_path).read_text())
old = root / 'rc/xattr-fault-20260925'
new = root / 'rc/post-recovery-le-20260925.bin'
payload = bytes(range(256)) * 256 + b'LE after barrier VCI recovery\n'
value = b'\x00\xffLE-after-recovery'

def absent(path, key):
    try:
        os.getxattr(path, key)
    except OSError as e:
        assert e.errno == errno.ENODATA
    else:
        raise AssertionError(key + ' unexpectedly present')

for path, sha in manifest['hashes'].items():
    if mode == 'fuse-regular' and path == '/rc/link':
        print('GAP: /rc/link symlink read not supported; strict fuse mode remains failing', flush=True)
        continue
    assert hashlib.sha256((root / path.lstrip('/')).read_bytes()).hexdigest() == sha, path
assert old.read_bytes() == b'xattr fault unchanged data'
assert os.getxattr(old, 'user.cut') == b'uncommitted'
assert os.getxattr(old, 'user.set') == b'\x00\xffcommitted'
for name in ('barrier', 'vci', 'delete', 'powercut.binary', 'powercut.empty'):
    absent(old, 'user.' + name)
if mode == 'le':
    before_version = os.getxattr(root, 'user.ltfs.indexVersion').decode()
    assert before_version == '2.5.0', before_version
    assert not new.exists()
    with open(new, 'xb', buffering=0) as f:
        f.write(payload)
        os.fsync(f.fileno())
    os.setxattr(new, 'user.le.binary', value, os.XATTR_CREATE)
    os.setxattr(new, 'user.le.empty', b'', os.XATTR_CREATE)
    os.setxattr(old, 'user.le.after-recovery', value, os.XATTR_CREATE)
    os.setxattr(root, 'user.ltfs.sync', b'1')
    expected = dict(length=len(payload), sha256=hashlib.sha256(payload).hexdigest(),
                    mtime_ns=os.stat(new).st_mtime_ns, old_mtime_ns=os.stat(old).st_mtime_ns,
                    before_version=before_version,
                    after_version=os.getxattr(root, 'user.ltfs.indexVersion').decode())
    Path(manifest_path + '.expected').write_text(json.dumps(expected, indent=2))
    print(json.dumps(expected), flush=True)
else:
    expected = json.loads(Path(manifest_path + '.expected').read_text())
    assert os.stat(new).st_mtime_ns == expected['mtime_ns']
    assert os.stat(old).st_mtime_ns == expected['old_mtime_ns']
assert new.read_bytes() == payload
assert os.getxattr(new, 'user.le.binary') == value
assert os.getxattr(new, 'user.le.empty') == b''
assert os.getxattr(old, 'user.le.after-recovery') == value
fd = os.open(new, os.O_RDONLY)
try:
    assert os.pread(fd, 4097, 255) == payload[255:4352]
    with mmap.mmap(fd, 0, flags=mmap.MAP_SHARED, prot=mmap.PROT_READ) as view:
        assert view[:] == payload
finally:
    os.close(fd)
print('PASS', mode, 'baseline hashes=' + str(12 if mode == 'fuse-regular' else 13),
      'recovered attrs, new file, binary/empty xattrs, pread/mmap', flush=True)
