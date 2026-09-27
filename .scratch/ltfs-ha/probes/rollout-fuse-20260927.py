"""正式升级回归，仅操作 upgrade-20260927 专用目录。"""
import errno,hashlib,json,mmap,os,sys
from pathlib import Path
assert os.environ.get('TAPE_RS_ROLLOUT')=='formal-rc-20260927'
mode=sys.argv[1];assert mode in ('prepare','verify')
base=Path('/home/rocky/tape-rs-rollout-20260927');root=base/'mnt'
assert os.path.ismount(root)
p=root/'rc/upgrade-20260927';data=p/'data'
a,m=1234567890123456789,1234567891987654321
payload=hashlib.shake_256(b'tape-rs formal upgrade 20260927').digest(8*2**20)
if mode=='prepare':
 assert data.read_bytes()==payload
 (p/'directory').mkdir()
 (p/'directory').rename(p/'renamed-directory')
 os.symlink('data',p/'link')
 os.setxattr(data,'user.rollout.binary',b'\x00\xffrollout')
 with data.open('r+b',buffering=0) as f:
  os.fchmod(f.fileno(),0o400)
  os.utime(f.fileno(),ns=(a,m))
 os.utime(p/'renamed-directory',ns=(a,m))
 os.chmod(p/'renamed-directory',0o500)
 os.utime(p/'link',ns=(a,m),follow_symlinks=False)
 os.chown(data,12347,12348)
 os.setxattr(root,'user.ltfs.sync',b'1')
for path,perm in [(data,0o444),(p/'renamed-directory',0o555),(p/'link',0o777)]:
 st=path.lstat();assert st.st_mode&0o777==perm,(path,oct(st.st_mode));assert st.st_atime_ns==a and st.st_mtime_ns==m,(path,st)
 assert st.st_uid==st.st_gid==0
assert os.readlink(p/'link')=='data'
assert not (p/'directory').exists()
assert os.getxattr(data,'user.rollout.binary')==b'\x00\xffrollout'
for path in (data,p/'link'):
 with path.open('rb') as f:
  assert f.read()==payload
  with mmap.mmap(f.fileno(),0,access=mmap.ACCESS_READ) as mm:assert mm[:]==payload
assert os.getxattr(data,'user.tape.barcode')==b'TS1000L08'
try:os.link(data,p/'hardlink')
except OSError as e:assert e.errno==errno.EOPNOTSUPP,e
else:raise AssertionError('hardlink accepted')
print(json.dumps(dict(mode=mode,sha256=hashlib.sha256(payload).hexdigest(),bytes=len(payload),readonly=True,nanosecond_times=True,xattr=True,symlink=True,rename=True,sync=True,passed=True)),flush=True)
