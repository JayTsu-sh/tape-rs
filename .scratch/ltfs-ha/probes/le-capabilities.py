#!/usr/bin/env python3
"""Explicit LE/Holo probe; writes only the supplied codex-le-capabilities-* child.
Run prepare, normal LE unmount/mount, verify, cleanup. No device commands here.
"""
import ctypes
import errno
import hashlib
import json
import mmap
import os
from pathlib import Path
import shutil
import sys
import time
import traceback

root = Path(sys.argv[2])
phase = sys.argv[1]
assert root.name.startswith('codex-le-capabilities-') and root.is_absolute()
assert not root.is_symlink()
volume = root.parent
results = []

def check(name, fn):
    start = time.monotonic()
    try:
        detail = fn()
        row = dict(name=name, status='PASS', detail=detail)
    except Exception as e:
        row = dict(name=name, status='FAIL', error=str(e), errno=getattr(e, 'errno', None), traceback=traceback.format_exc())
    row['seconds'] = round(time.monotonic()-start, 4)
    results.append(row)
    print(json.dumps(row, ensure_ascii=False), flush=True)

def expect_errno(fn, allowed):
    try:
        fn()
    except OSError as e:
        assert e.errno in allowed, e
        return e.errno
    raise AssertionError('expected failure')

def generation():
    return os.getxattr(volume, 'user.ltfs.indexGeneration').decode()

def mmap_readonly():
    p = root/'mmap'
    data = bytes(range(256))*256
    p.write_bytes(data)
    fd = os.open(p, os.O_RDONLY)
    shared = mmap.mmap(fd, 0, flags=mmap.MAP_SHARED, prot=mmap.PROT_READ)
    private = mmap.mmap(fd, 0, flags=mmap.MAP_PRIVATE, prot=mmap.PROT_READ)
    try:
        assert shared[:] == private[:] == os.pread(fd, len(data), 0) == data
        os.close(fd); fd = -1
        assert shared[:] == private[:] == data
        return dict(bytes=len(data), sha256=hashlib.sha256(shared).hexdigest(), shared=True, private=True, after_fd_close=True)
    finally:
        if fd >= 0: os.close(fd)
        shared.close(); private.close()

def io_calls():
    p = root/'io'
    fd = os.open(p, os.O_CREAT|os.O_EXCL|os.O_RDWR, 0o600)
    try:
        assert os.write(fd,b'abcdefgh') == 8
        assert os.pwrite(fd,b'XY',2) == 2
        assert os.pread(fd,8,0) == b'abXYefgh'
        assert os.lseek(fd,0,os.SEEK_SET) == 0
        assert os.read(fd,8) == b'abXYefgh'
        os.lseek(fd,0,os.SEEK_SET)
        assert os.writev(fd,[b'12',b'34']) == 4
        os.lseek(fd,0,os.SEEK_SET)
        buffers = [bytearray(3),bytearray(5)]
        assert os.readv(fd,buffers) == 8
        assert bytes(buffers[0]+buffers[1]) == b'1234efgh'
        dup = os.dup(fd)
        target = os.open('/dev/null', os.O_RDONLY)
        try:
            os.dup2(fd,target)
            os.lseek(dup,1,os.SEEK_SET)
            assert os.read(target,2) == b'23'
            assert os.lseek(fd,0,os.SEEK_CUR) == 3
        finally:
            os.close(dup); os.close(target)
        assert os.fstat(fd).st_size == 8
        assert os.stat(p).st_ino == os.fstat(fd).st_ino
    finally: os.close(fd)
    # Direct creat(2) entry point, not only Python O_CREAT.
    libc = ctypes.CDLL(None,use_errno=True)
    fd = libc.creat(os.fsencode(root/'creat'),0o600)
    assert fd >= 0, ctypes.get_errno()
    os.close(fd)
    return 'open/creat/close/read/write/pread/pwrite/readv/writev/lseek/dup/dup2/stat/fstat'

def multiple_writers():
    p = root/'multi'
    p.write_bytes(b'A'*8192)
    first = os.open(p,os.O_RDWR)
    second = os.open(p,os.O_WRONLY|os.O_APPEND)
    try:
        assert os.pwrite(first,b'B',100) == 1
        assert os.write(second,b'TAIL') == 4
        assert os.pread(first,5,8191) == b'ATAIL'
        assert os.pread(first,1,100) == b'B'
        os.close(first); first = -1
        assert os.write(second,b'END') == 3
    finally:
        if first >= 0: os.close(first)
        os.close(second)
    assert p.read_bytes()[8192:] == b'TAILEND'
    return 'two writers; random overwrite and append; closing one preserves the other'

def truncate_sparse():
    p = root/'sparse'
    p.write_bytes(b'12345678')
    fd = os.open(p,os.O_RDWR)
    try:
        os.ftruncate(fd,3)
        assert os.pread(fd,99,0) == b'123'
        os.truncate(p,8192)
        assert os.pread(fd,8189,3) == b'\0'*8189
        offset = (1<<32)+123
        assert os.lseek(fd,offset,os.SEEK_SET) == offset
        os.write(fd,b'Z')
        assert os.fstat(fd).st_size == offset+1
        assert os.pread(fd,5,offset-4) == b'\0'*4+b'Z'
        os.ftruncate(fd,16)
        assert os.pread(fd,16,0) == b'123'+b'\0'*13
    finally: os.close(fd)
    return 'shrink/grow/zero-filled holes; 64-bit offset >4 GiB; final size 16'

def rename_file():
    src,dst = root/'rename-source',root/'rename-target'
    src.write_bytes(b'SOURCE'); dst.write_bytes(b'TARGET')
    source_ino = src.stat().st_ino
    source_fd = os.open(src,os.O_RDWR); target_fd = os.open(dst,os.O_RDONLY)
    try:
        os.rename(src,dst)
        assert not src.exists() and dst.stat().st_ino == source_ino
        assert dst.read_bytes() == b'SOURCE'
        assert os.pread(target_fd,99,0) == b'TARGET'
        assert os.pread(source_fd,99,0) == b'SOURCE'
        os.pwrite(source_fd,b'new',0)
        assert dst.read_bytes() == b'newRCE'
        os.unlink(dst)
        assert os.pread(source_fd,99,0) == b'newRCE'
    finally: os.close(source_fd); os.close(target_fd)
    return 'overwrite preserves source inode; old target fd preserved; write after rename; unlink open writer'

def directories():
    parent = root/'dir'; parent.mkdir()
    child = parent/'child'; child.mkdir(); (child/'file').write_bytes(b'child')
    old = child.stat().st_ino
    os.rename(child,root/'moved-dir')
    assert (root/'moved-dir').stat().st_ino == old
    assert (root/'moved-dir'/'file').read_bytes() == b'child'
    empty = root/'empty'; empty.mkdir()
    os.rename(root/'moved-dir',empty)
    assert (empty/'file').read_bytes() == b'child'
    other = root/'nonempty'; other.mkdir(); (other/'keep').write_bytes(b'keep')
    rejected = expect_errno(lambda: os.rename(empty,other), (errno.ENOTEMPTY,errno.EEXIST))
    assert (empty/'file').exists() and (other/'keep').exists()
    assert {'dir','empty','nonempty'}.issubset(set(os.listdir(root)))
    cwd = os.open('.',os.O_RDONLY)
    dfd = os.open(root,os.O_RDONLY|os.O_DIRECTORY)
    try:
        os.chdir(root); assert Path.cwd() == root
        os.chdir('/'); os.fchdir(dfd); assert Path.cwd() == root
    finally: os.fchdir(cwd); os.close(cwd); os.close(dfd)
    os.rmdir(parent)
    return dict(nonempty_rename_errno=rejected, directory_inode_preserved=True)

def chroot_child():
    pid = os.fork()
    if pid == 0:
        try:
            os.chroot(root); os.chdir('/')
            assert Path('/io').read_bytes() == b'1234efgh'
            os._exit(0)
        except BaseException: os._exit(1)
    _, status = os.waitpid(pid,0)
    assert os.WIFEXITED(status) and os.WEXITSTATUS(status) == 0, status
    return 'chroot restricted to child process'

def symlinks():
    link = root/'symlink'
    os.symlink('io',link)
    assert os.readlink(link) == 'io'
    assert os.path.islink(link) and link.lstat().st_ino != link.stat().st_ino
    assert link.read_bytes() == b'1234efgh'
    os.lchown(link,12345,12346)
    dangling = root/'dangling'; os.symlink('absent',dangling)
    assert os.readlink(dangling) == 'absent'
    return dict(link_uid=link.lstat().st_uid, link_gid=link.lstat().st_gid)

def permissions():
    p = root/'permissions'; p.write_bytes(b'permission')
    fd = os.open(p,os.O_RDWR)
    detail = {}
    try:
        os.chmod(p,0o400); detail['readonly_mode'] = oct(p.stat().st_mode & 0o777)
        def try_write():
            f = os.open(p,os.O_WRONLY)
            try: os.write(f,b'X')
            finally: os.close(f)
        try:
            try_write()
            detail['root_write_to_readonly'] = 'allowed'
        except OSError as e:
            detail['root_write_to_readonly'] = dict(errno=e.errno)
        os.fchmod(fd,0o777); detail['requested_0777_mode'] = oct(p.stat().st_mode & 0o777)
        os.chown(p,12345,12346)
        os.fchown(fd,12347,12348)
        detail['requested_owner_12347_12348'] = [p.stat().st_uid,p.stat().st_gid]
    finally:
        os.fchmod(fd,0o600); os.close(fd)
    return detail

def attributes():
    p = root/'xattr'; p.write_bytes(b'attrs')
    key = 'user.codex.binary'; value = b'\0\xff'+ '中文'.encode()
    os.setxattr(p,key,value,os.XATTR_CREATE)
    assert os.getxattr(p,key) == value and key in os.listxattr(p)
    exists = expect_errno(lambda: os.setxattr(p,key,b'bad',os.XATTR_CREATE),(errno.EEXIST,))
    os.setxattr(p,key,b'replaced',os.XATTR_REPLACE)
    assert os.getxattr(p,key) == b'replaced'
    os.removexattr(p,key)
    missing = expect_errno(lambda: os.getxattr(p,key),(errno.ENODATA,))
    os.setxattr(p,'user.codex.persist',b'persisted')
    return dict(create_existing_errno=exists, removed_errno=missing)

def statistics():
    p = root/'io'; fd = os.open(p,os.O_RDONLY)
    try:
        # statfs/fstatfs ABI: opaque oversized aligned buffer, only inspect return codes.
        libc = ctypes.CDLL(None,use_errno=True)
        buf = (ctypes.c_long * 128)()
        assert libc.statfs(os.fsencode(root),ctypes.byref(buf)) == 0, ctypes.get_errno()
        assert libc.fstatfs(fd,ctypes.byref(buf)) == 0, ctypes.get_errno()
        st = os.statvfs(root)
        return dict(block_size=st.f_bsize, blocks=st.f_blocks, free=st.f_bfree)
    finally: os.close(fd)

def synchronization():
    p = root/'sync'; fd = os.open(p,os.O_CREAT|os.O_RDWR,0o600)
    detail = dict(before=generation())
    try:
        os.write(fd,b'sync-data')
        os.fdatasync(fd); detail['after_fdatasync'] = generation()
        os.fsync(fd); detail['after_fsync'] = generation()
        os.sync(); detail['after_sync'] = generation()
        # LE virtual root attribute: explicit index sync, arbitrary value.
        os.setxattr(volume,'user.ltfs.sync',b'1')
        detail['after_ltfs_sync'] = generation()
        assert int(detail['after_ltfs_sync']) > int(detail['before']), detail
    finally: os.close(fd)
    return detail

def concurrent_append():
    p = root/'concurrent-append'
    p.write_bytes(b'')
    children = []
    for tag in (b'A',b'B'):
        pid = os.fork()
        if pid == 0:
            try:
                fd = os.open(p,os.O_WRONLY|os.O_APPEND)
                for n in range(64):
                    record = tag + str(n).encode().rjust(7,b'0')
                    assert os.write(fd,record) == 8
                os.close(fd); os._exit(0)
            except BaseException: os._exit(1)
        children.append(pid)
    for pid in children:
        _, status = os.waitpid(pid,0)
        assert os.WIFEXITED(status) and os.WEXITSTATUS(status) == 0, status
    data = p.read_bytes()
    expected = {tag+str(n).encode().rjust(7,b'0') for tag in (b'A',b'B') for n in range(64)}
    assert len(data) == 1024 and {data[i:i+8] for i in range(0,len(data),8)} == expected
    return 'two concurrent processes; all 128 append records intact and unique'

def namespace_unmount():
    mountpoint = str(volume.parent)
    assert mountpoint == '/ltfs'
    def entry():
        return [line for line in Path('/proc/self/mountinfo').read_text().splitlines() if line.split()[4] == mountpoint]
    before = entry()
    assert len(before) == 1
    for operation in ('umount','umount2'):
        pid = os.fork()
        if pid == 0:
            try:
                libc = ctypes.CDLL(None,use_errno=True)
                # A private mount namespace is mandatory: never unmount the EE host mount.
                assert libc.unshare(0x00020000) == 0, ctypes.get_errno()  # CLONE_NEWNS
                assert libc.mount(None,b'/',None,ctypes.c_ulong((1<<18)|16384),None) == 0, ctypes.get_errno()
                assert entry()
                rc = libc.umount(b'/ltfs') if operation == 'umount' else libc.umount2(b'/ltfs',0)
                assert rc == 0, ctypes.get_errno()
                assert not entry()
                os._exit(0)
            except BaseException: os._exit(1)
        _, status = os.waitpid(pid,0)
        assert os.WIFEXITED(status) and os.WEXITSTATUS(status) == 0, (operation,status)
        assert entry() == before and volume.is_dir()
    return 'umount and umount2 succeed in isolated private namespaces; host /ltfs unchanged'

def persisted():
    assert (root/'io').read_bytes() == b'1234efgh'
    assert (root/'sparse').read_bytes() == b'123'+b'\0'*13
    assert (root/'empty'/'file').read_bytes() == b'child'
    assert (root/'nonempty'/'keep').read_bytes() == b'keep'
    assert os.readlink(root/'symlink') == 'io'
    assert os.getxattr(root/'xattr','user.codex.persist') == b'persisted'
    assert (root/'sync').read_bytes() == b'sync-data'
    assert (root/'persistent-empty').is_dir()
    return dict(generation=generation(), files_checked=8)

if phase == 'prepare':
    root.mkdir(mode=0o700)
    for name, fn in [('readonly_mmap',mmap_readonly),('file_io',io_calls),('multi_writer_append',multiple_writers),('truncate_sparse_64bit',truncate_sparse),('rename_file_unlink_open',rename_file),('directory_operations',directories),('chroot_child',chroot_child),('symlink_readlink_lstat_lchown',symlinks),('permissions',permissions),('xattrs',attributes),('statfs_fstatfs',statistics)]:
        check(name,fn)
    (root/'persistent-empty').mkdir()
    check('sync_boundaries',synchronization)
elif phase == 'extended':
    check('concurrent_append',concurrent_append)
    check('umount_umount2_private_namespace',namespace_unmount)
elif phase == 'permissions':
    check('permissions_observed_as_root',permissions)
elif phase == 'verify':
    check('after_normal_remount',persisted)
    def remap():
        fd = os.open(root/'mmap',os.O_RDONLY)
        m = mmap.mmap(fd,0,flags=mmap.MAP_SHARED,prot=mmap.PROT_READ)
        os.close(fd)
        try:
            assert m[:] == bytes(range(256))*256
            return hashlib.sha256(m).hexdigest()
        finally: m.close()
    check('readonly_mmap_after_remount',remap)
elif phase == 'cleanup':
    shutil.rmtree(root)
    assert not root.exists()
    print(json.dumps(dict(name='cleanup',status='PASS',root=str(root))))
else: raise ValueError(phase)
sys.exit(1 if any(r['status']=='FAIL' for r in results) else 0)
