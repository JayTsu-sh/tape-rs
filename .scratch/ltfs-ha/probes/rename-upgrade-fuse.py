# 显式门控：仅用于原 Holo 集群 rename 升级后的专用验收路径。
import os, sys, mmap, ctypes, errno
mount = os.environ['TAPE_RS_RENAME_UPGRADE_MOUNT']
assert mount == '/home/rocky/tape-rs-rename-prod-20260924/mnt'
assert os.path.ismount(mount)
p = mount + '/rc/rename-upgrade-20260924'
mode = sys.argv[1]
if mode == 'prepare':
    assert not os.path.lexists(p)
    os.mkdir(p)
    os.mkdir(p + '/source')
    os.mkdir(p + '/source/empty')
    fd = os.open(p + '/source/file', os.O_CREAT | os.O_EXCL | os.O_RDWR, 0o600)
    try:
        os.write(fd, b'before rename'); os.fsync(fd)
        ino = os.fstat(fd).st_ino
        os.rename(p + '/source', p + '/target')
        assert os.stat(p + '/target/file').st_ino == ino
        os.pwrite(fd, b'after--rename', 0); os.fsync(fd)
        os.rename(p + '/target/file', p + '/target/renamed')
        assert os.stat(p + '/target/renamed').st_ino == ino
        libc = ctypes.CDLL(None, use_errno=True)
        assert libc.renameat2(-100, (p+'/target/renamed').encode(), -100, (p+'/target/renamed').encode(), 1) == -1
        assert ctypes.get_errno() == errno.EEXIST
    finally:
        os.close(fd)
if mode in ('prepare', 'verify', 'cleanup'):
    assert not os.path.exists(p + '/source')
    assert not os.path.exists(p + '/target/file')
    assert os.path.isdir(p + '/target/empty')
    with open(p + '/target/renamed', 'rb') as f:
        assert f.read() == b'after--rename'
        with mmap.mmap(f.fileno(), 0, flags=mmap.MAP_SHARED, prot=mmap.PROT_READ) as m:
            assert m[:] == b'after--rename'
else:
    raise ValueError(mode)
if mode == 'cleanup':
    os.unlink(p + '/target/renamed')
    os.rmdir(p + '/target/empty')
    os.rmdir(p + '/target')
    os.rmdir(p)
    assert not os.path.exists(p)
print('PASS rename upgrade', mode, flush=True)
