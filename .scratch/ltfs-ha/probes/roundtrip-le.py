import os,sys,json,hashlib
root='/ltfs/TS1000L8'
mode=sys.argv[1]
base=bytes(range(256))*256
if mode=='prepare':
 os.mkdir(root+'/rc')
 for name in ['keep.bin','edit.bin']:
  p=root+'/rc/'+name
  with open(p,'xb') as f: f.write(base); f.flush(); os.fsync(f.fileno())
  os.setxattr(p,'user.roundtrip.binary',b'\x00\xffLE-roundtrip')
  os.utime(p,ns=(1790200000123456789,1790200000123456789))
 os.mkdir(root+'/rc/empty')
 os.symlink('keep.bin',root+'/rc/link')
 os.setxattr(root,'user.ltfs.sync',b'1')
 print('PREPARED',os.getxattr(root,'user.ltfs.indexVersion').decode())
else:
 expected=bytearray(base);expected[4093:4093+len(b'FUSE-ROUNDTRIP!!')]=b'FUSE-ROUNDTRIP!!'
 expected.extend(b'\x00'*(70000-len(expected)))
 expected.extend(b'APPEND-FUSE\n')
 missing=[]
 for name,data in [('keep.bin',base),('edit.bin',bytes(expected)),('new.bin',b'created through tape-fuse\n')]:
  p=root+'/rc/'+name
  got=open(p,'rb').read();assert got==data,(name,len(got),len(data))
  print(name,'bytes',len(got),'sha256',hashlib.sha256(got).hexdigest(),'mtime',os.stat(p).st_mtime_ns)
  if name!='new.bin':
   attrs={k:os.getxattr(p,k).hex() for k in os.listxattr(p) if k=='user.roundtrip.binary'}
   print('XATTR',name,attrs)
   if attrs.get('user.roundtrip.binary') != b'\x00\xffLE-roundtrip'.hex(): missing.append(name)
 assert os.stat(root+'/rc/keep.bin').st_mtime_ns==1790200000123456789
 assert os.path.isdir(root+'/rc/empty') and os.listdir(root+'/rc/empty')==[]
 assert os.readlink(root+'/rc/link')=='keep.bin'
 print('PASS LE content/keep mtime/empty directory/symlink; index',os.getxattr(root,'user.ltfs.indexVersion').decode(),flush=True)
 assert not missing, 'LE confirms xattr lost: '+repr(missing)
