"""显式挂载路径门控；仅操作本轮隔离测试目录。"""
import errno
import hashlib
import json
import os
import sys

mode, root, state = sys.argv[1:]
assert os.environ.get('TAPE_RS_XATTR_LAB') == 'RT2502L8-TS1000L8'
assert root in ('/home/rocky/tape-rs-xattr-20260924/mnt', '/ltfs/TS1000L8')
assert os.path.ismount('/ltfs' if root == '/ltfs/TS1000L8' else root)
p = root + '/rc/xattr-interop-20260924'
f = p + '/file'
v = b'\x00\xff<>&binary' + bytes(range(256))
def absent(path, name):
    try:
        os.getxattr(path, name)
    except OSError as e:
        assert e.errno == errno.ENODATA, e
    else:
        raise AssertionError('attribute unexpectedly exists')
def check():
    e = json.load(open(state))
    assert hashlib.sha256(open(f, 'rb').read()).hexdigest() == e['sha256']
    assert os.stat(f).st_mtime_ns == e['mtime']
if mode == 'prepare':
    os.mkdir(p)
    with open(f, 'xb', buffering=0) as out:
        out.write(b'xattr interop content\n')
        os.fsync(out.fileno())
    e = dict(sha256=hashlib.sha256(open(f, 'rb').read()).hexdigest(), mtime=os.stat(f).st_mtime_ns)
    with open(state, 'x') as out:
        json.dump(e, out)
    os.setxattr(f, 'user.binary', v, os.XATTR_CREATE)
    os.setxattr(f, 'user.empty', b'')
    os.setxattr(f, 'user.remove', b'delete from LE')
    os.setxattr(f, 'user.native-deleted', b'temporary')
    os.removexattr(f, 'user.native-deleted')
    os.setxattr(p, 'user.directory', v)
    assert os.getxattr(f, 'user.binary') == v
    absent(f, 'user.native-deleted')
    check()
elif mode == 'le':
    check()
    assert os.getxattr(f, 'user.binary') == v
    assert os.getxattr(f, 'user.empty') == b''
    assert os.getxattr(p, 'user.directory') == v
    absent(f, 'user.native-deleted')
    os.setxattr(f, 'user.binary', b'\xff\x00LE-replaced', os.XATTR_REPLACE)
    os.removexattr(f, 'user.remove')
    os.setxattr(f, 'user.le-created', b'\x00\xfffrom LE', os.XATTR_CREATE)
    os.removexattr(p, 'user.directory')
    os.setxattr(p, 'user.le-directory', b'\xff\x00directory')
    os.setxattr(root, 'user.ltfs.sync', b'1')
elif mode == 'verify':
    check()
    assert os.getxattr(f, 'user.binary') == b'\xff\x00LE-replaced'
    assert os.getxattr(f, 'user.le-created') == b'\x00\xfffrom LE'
    assert os.getxattr(f, 'user.empty') == b''
    assert os.getxattr(p, 'user.le-directory') == b'\xff\x00directory'
    for name in ('user.remove', 'user.native-deleted'):
        absent(f, name)
    absent(p, 'user.directory')
else:
    raise AssertionError(mode)
print('PASS writable xattr', mode, flush=True)
