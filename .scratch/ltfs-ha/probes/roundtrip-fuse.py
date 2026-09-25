import os,json,hashlib,urllib.request
root='/home/rocky/tape-rs-roundtrip-20260924/mnt'
base=bytes(range(256))*256
p=root+'/rc/edit.bin'
assert open(p,'rb').read()==base
assert os.getxattr(p,'user.roundtrip.binary')==b'\x00\xffLE-roundtrip'
fd=os.open(p,os.O_RDWR)
os.pwrite(fd,b'FUSE-ROUNDTRIP!!',4093)
os.ftruncate(fd,70000)
os.fsync(fd)
os.close(fd)
with open(p,'ab') as f:f.write(b'APPEND-FUSE\n');f.flush();os.fsync(f.fileno())
with open(root+'/rc/new.bin','xb') as f:f.write(b'created through tape-fuse\n');f.flush();os.fsync(f.fileno())
for name in ['keep.bin','edit.bin','new.bin']:
 p=root+'/rc/'+name
 print(name,os.stat(p).st_size,hashlib.sha256(open(p,'rb').read()).hexdigest(),os.stat(p).st_mtime_ns,flush=True)
 try:print('xattr',os.getxattr(p,'user.roundtrip.binary').hex(),flush=True)
 except OSError as e:print('xattr error',str(e),flush=True)
print('FUSE WRITES SYNCED',flush=True)
