"""回收后介质：IBM LE 写回与 tape-rs FUSE 读回。"""
import errno
import hashlib
import json
import mmap
import os
from pathlib import Path
import sys

assert os.environ.get('TAPE_RS_RECLAIM_LE') == 'SR2501L8-IBM-LE'
mode = sys.argv[1]
assert mode in ('le', 'fuse')
root = Path('/ltfs/SR2501L08' if mode == 'le' else '/home/rocky/tape-rs-reclaim-le-20260927/mnt')
assert os.path.ismount(root.parent if mode == 'le' else root)
links = {'relative':'target', 'dangling':'../absent & 中文%?#', 'directory':'subdir', 'loop':'loop', '中文':'target', 'chain':'relative'}
p = root/'sr/links'
for name,target in links.items():
    assert os.path.islink(p/name) and os.readlink(p/name)==target
    assert os.lstat(p/name).st_size==len(target.encode())
for name in ('target','relative','中文','chain'):
    assert (p/name).read_bytes()==b'created through symlink\n'
for name,code in [('dangling',errno.ENOENT),('loop',errno.ELOOP)]:
    try: (p/name).read_bytes()
    except OSError as e: assert e.errno==code,(name,e)
    else: raise AssertionError(name)
assert (p/'directory').is_dir()
assert hashlib.sha256((root/'sr/seed').read_bytes()).hexdigest()=='ba3378d7acafca6ec617933b2ea1bdb95bbdb2db4dd4dd6f528ec07cdf827aa7'
new = root/'sr/le-reclaimed-20260927.bin'
payload = bytes(range(256))*256+b'IBM LE after reclaim\n'
value = b'\x00\xffreclaimed LE'
if mode=='le':
    assert not new.exists()
    with open(new,'xb',buffering=0) as f:
        f.write(payload)
        os.fsync(f.fileno())
    os.setxattr(new,'user.le.binary',value,os.XATTR_CREATE)
    os.setxattr(new,'user.le.empty',b'',os.XATTR_CREATE)
    os.setxattr(p/'target','user.le.reclaimed',value,os.XATTR_CREATE)
    os.setxattr(root,'user.ltfs.sync',b'1')
assert new.read_bytes()==payload
assert os.getxattr(new,'user.le.binary')==value
assert os.getxattr(new,'user.le.empty')==b''
assert os.getxattr(p/'target','user.le.reclaimed')==value
with open(new,'rb') as f:
    assert os.pread(f.fileno(),4097,255)==payload[255:4352]
    with mmap.mmap(f.fileno(),0,flags=mmap.MAP_SHARED,prot=mmap.PROT_READ) as v:assert v[:]==payload
print(json.dumps(dict(mode=mode,length=len(payload),sha256=hashlib.sha256(payload).hexdigest(),mtime_ns=os.stat(new).st_mtime_ns,target_mtime_ns=os.stat(p/'target').st_mtime_ns,links=links,passed=True),ensure_ascii=False),flush=True)
