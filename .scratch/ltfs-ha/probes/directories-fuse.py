import os, sys, errno, hashlib, mmap
root=sys.argv[1]
p=root+'/rc/dir-interop-20260924'
if sys.argv[2]=='prepare':
    os.mkdir(p+'/fuse-empty')
    os.mkdir(p+'/nested')
    os.mkdir(p+'/nested/child')
    os.mkdir(p+'/fuse-remove')
    os.rmdir(p+'/fuse-remove')
    try: os.rmdir(p+'/nested')
    except OSError as e: assert e.errno==errno.ENOTEMPTY, e
    else: raise AssertionError('nonempty rmdir succeeded')
for n in ['fuse-empty','nested/child']:
    assert os.path.isdir(p+'/'+n)
    assert os.listdir(p+'/'+n)==[]
assert not os.path.exists(p+'/fuse-remove')
assert not os.path.exists(p+'/client-remove')
assert os.getxattr(root+'/rc/keep.bin','user.roundtrip.binary')==b'\x00\xffLE-roundtrip'
print('PASS FUSE directories',sys.argv[2],hashlib.sha256(open(root+'/rc/keep.bin','rb').read()).hexdigest(),flush=True)

if sys.argv[2]=='verify-le':
    assert not os.path.exists(p+'/client-empty')
    assert os.path.isdir(p+'/le-empty')
    assert os.listdir(p+'/le-empty')==[]
    assert os.getxattr(p+'/le-empty','user.directory.binary')==b'\x00\xffdirectory'
    assert os.stat(p+'/le-empty').st_mtime_ns==1790200000123456789
    with open(p+'/le-file','rb') as f:
        with mmap.mmap(f.fileno(),0,flags=mmap.MAP_SHARED,prot=mmap.PROT_READ) as m:
            assert m[:]==b'LE directory roundtrip\n'
    print('PASS LE directory deletion/new directory/binary xattr/mtime/readonly mmap')
