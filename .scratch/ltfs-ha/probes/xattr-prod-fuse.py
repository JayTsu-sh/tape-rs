"""Holo 原集群发布验收；仅写入专用目录，必须显式设置门控。"""
import errno
import hashlib
import json
import mmap
import os
import sys

assert os.environ.get('TAPE_RS_XATTR_PROD') == 'TS1001L08'
root = '/home/rocky/tape-rs-xattr-prod-20260924/mnt'
assert os.path.ismount(root)
p = root + '/rc/xattr-prod-20260924'
f = p + '/file'
mode = sys.argv[1]
v = b'\x00\xff<>&binary'
body = b'xattr production smoke\n'
def absent(path, key):
    try:
        os.getxattr(path, key)
    except OSError as e:
        assert e.errno == errno.ENODATA, e
    else:
        raise AssertionError('unexpected attribute')
def verify():
    assert open(f, 'rb').read() == body
    assert os.getxattr(f, 'user.binary') == v
    assert os.getxattr(f, 'user.empty') == b''
    assert os.getxattr(p, 'user.directory') == v
    assert 'user.directory' in os.listxattr(p)
    absent(f, 'user.removed')
    with open(f, 'rb') as stream:
        with mmap.mmap(stream.fileno(), 0, flags=mmap.MAP_SHARED, prot=mmap.PROT_READ) as m:
            assert m[:] == body
if mode == 'prepare':
    os.mkdir(p)
    with open(f, 'xb', buffering=0) as out:
        out.write(body)
        os.fsync(out.fileno())
    assert os.getxattr(f, 'user.tape.barcode') == b'TS1001L08'
    stamp = os.stat(f).st_mtime_ns
    os.setxattr(f, 'user.binary', b'initial', os.XATTR_CREATE)
    try:
        os.setxattr(f, 'user.binary', v, os.XATTR_CREATE)
    except OSError as e:
        assert e.errno == errno.EEXIST
    else:
        raise AssertionError('CREATE overwrote attribute')
    os.setxattr(f, 'user.binary', v, os.XATTR_REPLACE)
    os.setxattr(f, 'user.empty', b'')
    os.setxattr(f, 'user.removed', b'temporary')
    os.removexattr(f, 'user.removed')
    os.setxattr(p, 'user.directory', v)
    assert os.stat(f).st_mtime_ns == stamp
    verify()
elif mode == 'verify-cleanup':
    verify()
    os.removexattr(f, 'user.empty')
    absent(f, 'user.empty')
    os.removexattr(p, 'user.directory')
    absent(p, 'user.directory')
    os.unlink(f)
    os.rmdir(p)
    assert not os.path.exists(p)
elif mode == 'original':
    rows=json.load(open('/home/rocky/tape-rs-xattr-prod-20260924/original-files.json'))
    for r in rows:
        data=open(root+r['path'], 'rb').read()
        assert len(data)==r['length'] and hashlib.sha256(data).hexdigest()==r['sha256'], r
    assert not os.path.exists(p)
else:
    raise AssertionError(mode)
print('PASS xattr production FUSE', mode, flush=True)
