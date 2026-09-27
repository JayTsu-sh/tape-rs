"""生成双方共用的本地源文件及逐文件摘要，不访问设备。"""
import hashlib,json,os
from pathlib import Path
assert os.environ.get('TAPE_RS_PERF_COMPARE')=='PF2701L08-PF2702L08'
base=Path('/home/rocky/tape-rs-le-compare-20260927');base.mkdir(exist_ok=True)
source=base/'source';source.mkdir(exist_ok=False)
specs={
 'PF2701L08':('5c16daae-fa54-40d8-ba28-988f3e9fd15a',[('large',4,8*2**20),('small',256,4096),('growth',2500,64)]+[(f'continuous-{i:02}',32,4096) for i in range(12)]+[('mixed',64,32768)]),
 'PF2702L08':('bbdecc80-9c76-4bba-b82b-302b552ffefe',[('switch-volume',1,128*2**20),('switch-read',1,8*2**20)])}
for barcode,(uuid,groups) in specs.items():
 rows=[]
 for group,count,size in groups:
  for i in range(count):
   path=f'comparison/{group}/f{i:05}';data=hashlib.shake_256(path.encode()).digest(size)
   target=source/path;target.parent.mkdir(parents=True,exist_ok=True);target.write_bytes(data)
   rows.append(dict(path=path,group=group,size=size,sha256=hashlib.sha256(data).hexdigest()))
 (base/(barcode+'.json')).write_text(json.dumps(dict(barcode=barcode,volume_uuid=uuid,files=rows)))
 print(barcode,len(rows),sum(r['size'] for r in rows))
