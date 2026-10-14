import os,sys,hashlib
root='/ltfs/TS1000L8'
p=root+'/rc/dir-interop-20260924'
for n in ['client-empty','fuse-empty','nested/child']:
    assert os.path.isdir(p+'/'+n)
    assert os.listdir(p+'/'+n)==[]
for n in ['client-remove','fuse-remove']:
    assert not os.path.exists(p+'/'+n)
assert os.getxattr(root+'/rc/keep.bin','user.roundtrip.binary')==b'\x00\xffLE-roundtrip'
os.rmdir(p+'/client-empty')
os.mkdir(p+'/le-empty')
os.setxattr(p+'/le-empty','user.directory.binary',b'\x00\xffdirectory')
os.utime(p+'/le-empty',ns=(1790200000123456789,1790200000123456789))
with open(p+'/le-file','xb') as f:
    f.write(b'LE directory roundtrip\n'); f.flush(); os.fsync(f.fileno())
os.setxattr(root,'user.ltfs.sync',b'1')
print('PASS LE native directory read/write',hashlib.sha256(open(root+'/rc/keep.bin','rb').read()).hexdigest(),flush=True)
