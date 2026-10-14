import os,json,hashlib,time
root='/home/rocky/tape-rs-xattr-ha-20260924/mnt'
p=root+'/rc/edit.bin'
original=open(p,'rb').read()
assert os.getxattr(p,'user.roundtrip.binary')==b'\x00\xffLE-roundtrip'
marker=b'HA-FSYNC-RESPONSE-LOST'
expected=bytearray(original);expected[100:100+len(marker)]=marker
fd=os.open(p,os.O_RDWR)
assert os.pwrite(fd,marker,100)==len(marker)
print('FSYNC_START',time.time(),flush=True)
os.fsync(fd)
print('FSYNC_SUCCESS',time.time(),flush=True)
os.close(fd)
assert open(p,'rb').read()==expected
assert os.getxattr(p,'user.roundtrip.binary')==b'\x00\xffLE-roundtrip'
print('PASS fsync after lost response and leader failure',hashlib.sha256(expected).hexdigest(),flush=True)
open('/home/rocky/tape-rs-xattr-ha-20260924/fsync-expected.bin','wb').write(expected)
