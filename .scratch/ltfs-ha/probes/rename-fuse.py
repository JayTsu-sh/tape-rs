import os,sys,json,hashlib,errno,ctypes,mmap
root=os.environ['TAPE_RS_RENAME_MOUNT']
assert root=='/home/rocky/tape-rs-rename-20260924/mnt'
assert os.path.ismount(root)
p=root+'/rc/rename-interop-20260924'
mode=sys.argv[1]
state=sys.argv[2]
def digest(path):return hashlib.sha256(open(path,'rb').read()).hexdigest()
if mode=='prepare':
    original=root+'/rc/edit.bin'
    expected=dict(sha256=digest(original),mtime=os.stat(original).st_mtime_ns,xattr=os.getxattr(original,'user.roundtrip.binary').hex())
    with open(state,'x') as f:json.dump(expected,f)
    ino=os.stat(original).st_ino
    os.rename(original,p+'/imported.bin')
    assert os.stat(p+'/imported.bin').st_ino==ino
    assert digest(p+'/imported.bin')==expected['sha256']
    olddir=root+'/rc/dir-interop-20260924/le-empty'
    di=os.stat(olddir).st_ino
    os.rename(olddir,p+'/imported-empty')
    assert os.stat(p+'/imported-empty').st_ino==di
    os.mkdir(p+'/source');os.mkdir(p+'/source/empty')
    fd=os.open(p+'/source/file',os.O_CREAT|os.O_EXCL|os.O_RDWR,0o600)
    os.write(fd,b'open writer');os.fsync(fd)
    ino=os.fstat(fd).st_ino
    os.rename(p+'/source',p+'/moved')
    os.pwrite(fd,b'after rename',0);os.fsync(fd)
    assert os.stat(p+'/moved/file').st_ino==ino
    target=os.open(p+'/target',os.O_CREAT|os.O_EXCL|os.O_RDWR,0o600)
    os.write(target,b'old target');os.fsync(target)
    libc=ctypes.CDLL(None,use_errno=True)
    assert libc.renameat2(-100,(p+'/moved/file').encode(),-100,(p+'/target').encode(),1)==-1
    assert ctypes.get_errno()==errno.EEXIST
    os.rename(p+'/moved/file',p+'/target')
    os.pwrite(target,b'detached!!',0);os.fsync(target)
    assert open(p+'/target','rb').read()==b'after rename'
    os.pwrite(fd,b'final writer',0);os.fsync(fd)
    assert open(p+'/target','rb').read()==b'final writer'
    assert os.pread(target,10,0)==b'detached!!'
    os.close(target);os.close(fd)
    assert not os.path.exists(p+'/source')
    try:os.rmdir(p+'/moved')
    except OSError as e:assert e.errno==errno.ENOTEMPTY,e
    else:raise AssertionError('nonempty rmdir succeeded')
else:
    expected=json.load(open(state))
    original=root+'/rc/edit.bin' if mode=='verify-le' else p+'/imported.bin'
    assert digest(original)==expected['sha256']
    assert os.stat(original).st_mtime_ns==expected['mtime']
    assert os.getxattr(original,'user.roundtrip.binary').hex()==expected['xattr']
    directory=root+'/rc/dir-interop-20260924/le-empty' if mode=='verify-le' else p+'/imported-empty'
    assert os.path.isdir(directory)
    assert os.getxattr(directory,'user.directory.binary')==b'\x00\xffdirectory'
    assert os.stat(directory).st_mtime_ns==1790200000123456789
    tree=p+('/source' if mode=='verify-le' else '/moved')
    assert os.path.isdir(tree+'/empty')
    assert open(p+'/target','rb').read()==b'final writer'
    with open(p+'/target','rb') as f:
        with mmap.mmap(f.fileno(),0,flags=mmap.MAP_SHARED,prot=mmap.PROT_READ) as m:assert m[:]==b'final writer'
print('PASS FUSE rename',mode,flush=True)
