"""挂载根显式同步：open/close/远端暂存、属性、错误边界和冷挂载校验。"""
import errno
import hashlib
import http.client
import json
import os
from pathlib import Path
import sys

assert os.environ.get('TAPE_RS_SYNC_BOUNDARY')=='SR2501L8-isolated'
root=Path('/home/rocky/tape-rs-sync-boundary-20260927/mnt')
assert os.path.ismount(root)
mode=sys.argv[1];assert mode in ('prepare','verify')
d=root/'sr/sync-20260927'
expected={'open':b'updated open data','closed':b'closed data','remote':b'remote client data'}
if mode=='prepare':
    d.mkdir()
    f=os.open(d/'open',os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)
    try:
        os.write(f,b'initial open data')
        (d/'closed').write_bytes(expected['closed'])
        c=http.client.HTTPConnection('10.131.9.71',7501,timeout=20);c.request('GET','/cluster');leader=json.loads(c.getresponse().read())['leader'];c.close()
        c=http.client.HTTPConnection({1:'10.131.9.71',2:'10.131.9.72',3:'10.131.9.74'}[leader],7501,timeout=20)
        c.request('PUT','/files/sr/sync-20260927/remote',expected['remote']);r=c.getresponse();body=r.read();assert r.status in (201,202),(r.status,body);c.close()
        os.setxattr(root,'user.ltfs.sync',b'',os.XATTR_CREATE)
        assert (d/'open').read_bytes()==b'initial open data'
        assert (d/'closed').read_bytes()==expected['closed']
        assert (d/'remote').read_bytes()==expected['remote']
        os.pwrite(f,expected['open'],0);os.ftruncate(f,len(expected['open']))
        os.setxattr(d/'closed','user.sync.binary',b'\0\xffsync',os.XATTR_CREATE)
        os.setxattr(root,'user.ltfs.sync',b'any value',os.XATTR_REPLACE)
    finally:os.close(f)
    for target,code in [(d,errno.EACCES),(d/'closed',errno.EACCES)]:
        try:os.setxattr(target,'user.ltfs.sync',b'1')
        except OSError as e:assert e.errno==code,(target,e)
        else:raise AssertionError('non-root sync accepted')
    try:os.link(d/'closed',d/'hardlink')
    except OSError as e:assert e.errno==errno.EOPNOTSUPP,e
    else:raise AssertionError('hardlink unexpectedly created')
    assert not (d/'hardlink').exists()
    assert os.stat(d/'closed').st_nlink==1
    try:os.removexattr(root,'user.ltfs.sync')
    except OSError as e:assert e.errno==errno.EPERM,e
    else:raise AssertionError('virtual attribute removed')
for name,data in expected.items():assert (d/name).read_bytes()==data
assert os.getxattr(d/'closed','user.sync.binary')==b'\0\xffsync'
assert 'user.ltfs.sync' not in os.listxattr(root)
try:os.getxattr(root,'user.ltfs.sync')
except OSError as e:assert e.errno==errno.ENODATA,e
else:raise AssertionError('write-only attribute readable')
print(json.dumps(dict(mode=mode,files={k:hashlib.sha256(v).hexdigest() for k,v in expected.items()},passed=True)),flush=True)
