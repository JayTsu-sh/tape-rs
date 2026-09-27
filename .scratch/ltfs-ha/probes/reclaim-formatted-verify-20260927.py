"""只读复核已保存的隔离目录与中断证据。"""
from pathlib import Path
import json
base=Path(__file__).resolve().parent / 'results'
a=json.loads((base/'reclaim-formatted-20260927-isolated-before.json').read_text())['71']['files']
b=json.loads((base/'reclaim-formatted-20260927-isolated-after.json').read_text())['71']['files']
assert len(a)==len(b)==13
for x,y in zip(a,b):
 for k in ('pool_uuid','path','length','sha256','deleted'):assert x[k]==y[k],(x['path'],k)
 assert y['barcode']=='SR2502L8'
 xm=json.loads(x['metadata']);ym=json.loads(y['metadata'])
 for meta in (xm,ym):
  if meta is not None:meta['xattrs']=[v for v in meta.get('xattrs',[]) if v['key']!='tapers.version']
 assert xm==ym,(x['path'],xm,ym)
print('PASS 13 catalog rows: type, target, timestamp, xattrs, length, hash and tombstones preserved')
event=json.loads((base/'reclaim-formatted-20260927-event.json').read_text())
trace=event['trace_after_exit'].splitlines()
f=[i for i,l in enumerate(trace) if 'cmdp="\\x04' in l and ') = 0' in l and 'status=0,' in l]
assert len(f)==1
assert not any('cmdp="\\x08' in l and ') = 0' in l for l in trace[f[0]+1:]),'mount READ completed after mkltfs'
assert event['state_at_kill']['last_reclaim'] is None
assert next(t for t in event['state_at_kill']['tapes'] if t['barcode']=='SR2501L8')['state']=='reclaiming'
print('PASS source FORMAT GOOD, mkltfs complete, no remount READ completed and reclaiming at cut')
