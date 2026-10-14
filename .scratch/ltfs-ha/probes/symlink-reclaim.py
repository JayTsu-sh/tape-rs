"""两盘专用虚拟带的回收验收；不访问原集群。"""
import errno, hashlib, json, mmap, os, sys
from pathlib import Path
assert os.environ.get('TAPE_RS_SYMLINK_CREATE') == 'SR2501L8-SR2502L8'
mode = sys.argv[1]
assert mode in ('create', 'verify')
root = Path('/home/rocky/tape-rs-symlink-reclaim-20260925/mnt/sr')
assert os.path.ismount(root.parent)
p = root / 'links'
payload = b'created through symlink\n'
links = {'relative':'target', 'dangling':'../absent & 中文%?#', 'directory':'subdir', 'loop':'loop', '中文':'target', 'chain':'relative'}
if mode == 'create':
    p.mkdir()
    (p/'subdir').mkdir()
    with open(p/'target','xb',buffering=0) as f:
        f.write(b'before\n'); os.fsync(f.fileno())
    for name,target in links.items(): os.symlink(target,p/name)
    with open(p/'relative','wb',buffering=0) as f:
        f.write(payload); os.fsync(f.fileno())
    os.symlink('target',p/'temporary')
    os.rename(p/'temporary',p/'renamed')
    assert os.readlink(p/'renamed')=='target'
    (p/'renamed').unlink()
    assert (p/'target').read_bytes()==payload
    try: os.symlink('different',p/'relative')
    except FileExistsError: pass
    else: raise AssertionError('duplicate accepted')
for name,target in links.items():
    assert os.path.islink(p/name),name
    assert os.readlink(p/name)==target,(name,os.readlink(p/name))
    assert os.lstat(p/name).st_size==len(target.encode()),name
for name in ('target','relative','中文','chain'):
    assert (p/name).read_bytes()==payload,name
    with open(p/name,'rb') as f:
        with mmap.mmap(f.fileno(),0,access=mmap.ACCESS_READ) as m: assert m[:]==payload
for name,code in [('dangling',errno.ENOENT),('loop',errno.ELOOP)]:
    try: (p/name).read_bytes()
    except OSError as e: assert e.errno==code,(name,e)
    else: raise AssertionError(name)
assert (p/'directory').is_dir()
assert not os.path.lexists(p/'renamed')
assert {x.name for x in os.scandir(p) if x.is_symlink()}==set(links)
print(json.dumps(dict(mode=mode,links=links,sha256=hashlib.sha256(payload).hexdigest(),passed=True),ensure_ascii=False))

expected = 'SR2501L8' if mode=='create' else 'SR2502L8'
assert os.getxattr(p/'target','user.tape.barcode').decode()==expected
print('PASS target barcode',expected)
