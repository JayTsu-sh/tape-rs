"""隔离性能数据：冷缓存逐文件 SHA256、热缓存重复读取及目录枚举。"""
import concurrent.futures,hashlib,json,os,statistics,time,sys
from pathlib import Path
assert os.environ.get('TAPE_RS_PERFORMANCE')=='SR2501L8-isolated'
base=Path('/home/rocky/tape-rs-performance-20260927');root=base/'mnt'
assert os.path.ismount(root)
items=json.loads((base/'manifest.json').read_text())
if '--baseline' in sys.argv:
 assert len(items)>=3273
 items=items[:3273]
def read(row):
 data=(root/row['path'].lstrip('/')).read_bytes()
 assert len(data)==row['size'] and hashlib.sha256(data).hexdigest()==row['sha256'],row
 return len(data)
for label in ('cold','hot'):
 start=time.monotonic()
 with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:total=sum(pool.map(read,items))
 print(json.dumps(dict(kind='fuse-read',label=label,files=len(items),bytes=total,seconds=time.monotonic()-start,verified=True)),flush=True)
for directory in ('large','small','growth'):
 samples=[]
 for _ in range(10):
  start=time.monotonic();entries=list(os.scandir(root/('sr/performance-20260927-r1/'+directory)));samples.append((time.monotonic()-start)*1000)
 print(json.dumps(dict(kind='fuse-list',directory=directory,count=len(entries),median_ms=statistics.median(samples),samples_ms=samples)),flush=True)
