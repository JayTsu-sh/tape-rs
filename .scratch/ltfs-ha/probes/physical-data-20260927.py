"""生成实机双方共用的不可压缩源文件；不访问设备。UUID 由格式化后的 LE 状态填入。"""
import hashlib,json,os,shutil
from pathlib import Path
assert os.environ.get('TAPE_RS_PERF_COMPARE')=='RC0018L9-RC0017L9'
base=Path('/root/tape-rs-io-20260927');source=base/'source'
assert shutil.disk_usage(base).free>40*2**30
source.mkdir(exist_ok=False)
specs={
 'RC0018L9':[('large',4,256*2**20),('stream',1,16*2**30),('small',256,4096),('growth',2500,64)]+[(f'continuous-{i:02}',32,4096) for i in range(12)],
 'RC0017L9':[('switch-volume',1,2*2**30),('switch-read',1,256*2**20)]}
for barcode,groups in specs.items():
 rows=[]
 for group,count,size in groups:
  for i in range(count):
   path=f'comparison/{group}/f{i:05}';target=source/path;target.parent.mkdir(parents=True,exist_ok=True)
   h=hashlib.sha256();remaining=size
   with target.open('xb') as f:
    while remaining:
     data=os.urandom(min(2**20,remaining));f.write(data);h.update(data);remaining-=len(data)
   rows.append(dict(path=path,group=group,size=size,sha256=h.hexdigest()))
  print(barcode,group,count,size,flush=True)
 (base/(barcode+'.json')).write_text(json.dumps(dict(barcode=barcode,volume_uuid=None,files=rows)))
(base/'source.complete').write_text('source and sha256 ready')
