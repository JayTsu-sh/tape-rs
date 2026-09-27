"""大目录冷缓存内容验证；耗时与纯目录性能单独报告。"""
import json, os, time
from pathlib import Path
assert os.environ.get('TAPE_RS_DIRECTORY_PERF')=='SR2501L8-isolated'
base=Path('/home/rocky/tape-rs-directory-perf-20260927');root=base/'mnt'
assert os.path.ismount(root)
t=time.monotonic()
for n in (100,1000):
    d=root/f'sr/perf-20260927/{n}'
    for i in range(n):
        fd=os.open(d/f'f{i:05d}',os.O_RDONLY)
        try: assert os.read(fd,4096)==f'perf-{n}-{i:05d}\n'.encode()
        finally: os.close(fd)
    assert os.readlink(d/'link')=='f00000'
    assert os.readlink(d/'dangling')=='absent' and os.readlink(d/'loop')=='loop'
    print(json.dumps(dict(verified=n,seconds=time.monotonic()-t)),flush=True)
(base/'content-pass.json').write_text(json.dumps(dict(files=1100,seconds=time.monotonic()-t,passed=True))+'\n')
