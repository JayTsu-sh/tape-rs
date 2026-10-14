"""回收接管后只读检查：固定 SR 专用池与挂载点。"""
import errno, hashlib, json, mmap, os
from pathlib import Path
assert os.environ.get('TAPE_RS_RECLAIM_RAW') == 'SR2502L8-to-SR2501L8'
root = Path('/home/rocky/tape-rs-reclaim-raw-20260927/mnt/sr')
assert os.path.ismount(root.parent)
p = root / 'links'
payload = b'created through symlink\n'
links = {'relative':'target', 'dangling':'../absent & 中文%?#', 'directory':'subdir', 'loop':'loop', '中文':'target', 'chain':'relative'}
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
print(json.dumps(dict(links=links,sha256=hashlib.sha256(payload).hexdigest(),passed=True),ensure_ascii=False))

expected = 'SR2501L8'
assert os.getxattr(p/'target','user.tape.barcode').decode()==expected
print('PASS target barcode',expected)
assert hashlib.sha256((root / 'seed').read_bytes()).hexdigest() == 'ba3378d7acafca6ec617933b2ea1bdb95bbdb2db4dd4dd6f528ec07cdf827aa7'
print('PASS seed hash')
