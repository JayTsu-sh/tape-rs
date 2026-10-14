"""LE 专用 SR 副本：权限/时间 prepare、重挂 verify、cleanup；不执行设备命令。"""
import json,os,sys
from pathlib import Path
assert os.environ.get('TAPE_RS_LE_METADATA')=='SR2501L08-isolated'
root=Path('/ltfs/SR2501L08');assert os.path.ismount(root.parent)
p=root/'sr/metadata-20260927';mode=sys.argv[1];assert mode in ('prepare','verify','cleanup')
def attrs(x):
 s=x.lstat();return dict(mode=oct(s.st_mode&0o777),uid=s.st_uid,gid=s.st_gid,atime=s.st_atime_ns,mtime=s.st_mtime_ns,ctime=s.st_ctime_ns)
if mode=='prepare':
 p.mkdir();(p/'file').write_bytes(b'metadata proof');(p/'dir').mkdir();os.symlink('file',p/'link')
 for name in ('file','dir','link'):
  os.utime(p/name,ns=(1234567890123456789,1234567891987654321),follow_symlinks=False)
  os.chown(p/name,12347,12348,follow_symlinks=False)
 os.chmod(p/'file',0o400);os.chmod(p/'dir',0o500)
 os.setxattr(root,'user.ltfs.sync',b'1')
if mode in ('prepare','verify'):
 out={n:attrs(p/n) for n in ('file','dir','link')}
 assert all(v['atime']==1234567890123456789 and v['mtime']==1234567891987654321 for v in out.values()),out
 assert all(v['uid']==v['gid']==0 for v in out.values()),out
 assert not (int(out['file']['mode'],8)&0o222)
 print(json.dumps(dict(mode=mode,attrs=out,passed=True)),flush=True)
if mode=='cleanup':
 os.chmod(p/'file',0o777);os.chmod(p/'dir',0o777)
 print(json.dumps(dict(restored={n:attrs(p/n) for n in ('file','dir')})))
 (p/'link').unlink();(p/'file').unlink();(p/'dir').rmdir();p.rmdir();os.setxattr(root,'user.ltfs.sync',b'1')
