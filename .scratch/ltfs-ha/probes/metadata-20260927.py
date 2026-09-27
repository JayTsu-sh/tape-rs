"""原生元数据现场验收；仅固定隔离目录，LE阶段先核对再写回。"""
import errno,json,os,sys
from pathlib import Path
assert os.environ.get('TAPE_RS_METADATA')=='SR2501L8-SR2501L08'
mode=sys.argv[1];assert mode in ('prepare','verify','le','returned')
root=Path('/ltfs/SR2501L08' if mode=='le' else '/home/rocky/tape-rs-metadata-20260927/mnt')
assert os.path.ismount(root.parent if mode=='le' else root)
p=root/'sr/metadata-native-20260927'
a,m=1234567890123456789,1234567891987654321
if mode=='prepare':
 p.mkdir();(p/'dir').mkdir()
 f=os.open(p/'file',os.O_CREAT|os.O_EXCL|os.O_RDWR,0o600)
 try:
  os.write(f,b'metadata native content');os.fsync(f)
  os.utime(f,ns=(a,m));os.fchmod(f,0o400)
  assert os.fstat(f).st_mtime_ns==m
 finally:os.close(f)
 os.symlink('file',p/'link')
 for n in ('dir','link'):os.utime(p/n,ns=(a,m),follow_symlinks=False)
 os.chmod(p/'dir',0o500)
 for n in ('file','dir','link'):os.chown(p/n,12347,12348,follow_symlinks=False)
 os.setxattr(root,'user.ltfs.sync',b'1')
if mode=='returned':a,m=a+111,m+222
out={}
for n in ('file','dir','link'):
 s=(p/n).lstat();out[n]=dict(mode=oct(s.st_mode&0o777),atime=s.st_atime_ns,mtime=s.st_mtime_ns,uid=s.st_uid,gid=s.st_gid)
 assert (s.st_atime_ns,s.st_mtime_ns)==(a,m),(n,out[n])
 assert s.st_uid==s.st_gid==0
 if n!='link':assert s.st_mode&0o222==0,(n,out[n])
assert os.readlink(p/'link')=='file'
assert (p/'file').read_bytes()==b'metadata native content'
if mode=='le':
 for n in ('file','dir','link'):os.utime(p/n,ns=(a+111,m+222),follow_symlinks=False)
 os.setxattr(root,'user.ltfs.sync',b'1')
print(json.dumps(dict(mode=mode,attrs=out,passed=True)),flush=True)
