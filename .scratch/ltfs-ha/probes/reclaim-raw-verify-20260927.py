"""只读复核目录迁移与 FORMAT 后标签前的进程中断。"""
from pathlib import Path
import json
base = Path(__file__).resolve().parent / 'results'
a = json.loads((base/'reclaim-raw-20260927-isolated-before.json').read_text())['71']['files']
b = json.loads((base/'reclaim-raw-20260927-isolated-after.json').read_text())['71']['files']
assert len(a) == len(b) == 17
for x,y in zip(a,b):
    for k in ('pool_uuid','path','length','sha256','deleted'):
        assert x[k] == y[k], (x['path'],k)
    assert y['barcode'] == 'SR2501L8'
    xm,ym = json.loads(x['metadata']),json.loads(y['metadata'])
    for meta in (xm,ym):
        if meta is not None:
            meta['xattrs'] = [v for v in meta.get('xattrs',[]) if v['key'] != 'tapers.version']
    assert xm == ym, (x['path'],xm,ym)
print('PASS 17 catalog rows: metadata/content/tombstones preserved')
event = json.loads((base/'reclaim-raw-20260927-event.json').read_text())
trace = event['trace_after_exit'].splitlines()
formats = [i for i,l in enumerate(trace) if 'cmdp="\\x04' in l and ') = 0' in l and 'status=0,' in l]
assert len(formats) == 1
assert not any('cmdp="\\x0a' in l and ') = 0' in l for l in trace[formats[0]+1:])
assert event['state_at_kill']['last_reclaim'] is None
assert next(t for t in event['state_at_kill']['tapes'] if t['barcode']=='SR2502L8')['state']=='reformatting'
print('PASS FORMAT GOOD; no new label WRITE; durable reformatting phase')
