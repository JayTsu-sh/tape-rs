import os,mmap,hashlib,json
root='/home/rocky/tape-rs-le-downgrade-20260924/mnt'
p=root+'/rc/le-downgrade-20260924.bin'
expected=bytes(range(256))*256+b'written by actual IBM LE 2.4.8.3\n'
with open(p,'rb') as f:
 data=f.read(); assert data==expected
 assert os.pread(f.fileno(),4097,255)==expected[255:4352]
 with mmap.mmap(f.fileno(),0,flags=mmap.MAP_SHARED,prot=mmap.PROT_READ) as m:
  assert m[:]==expected
assert os.getxattr(p,'user.interop.binary')==b'\x00\xffLE24'
assert os.stat(p).st_mtime_ns==1790253484419335445
assert 'le-downgrade-20260924.bin' in os.listdir(root+'/rc')
print(json.dumps({'result':'PASS tape-fuse read/pread/readonly shared mmap/readdir/xattr/mtime','bytes':len(data),'sha256':hashlib.sha256(data).hexdigest(),'mtime_ns':os.stat(p).st_mtime_ns}),flush=True)
