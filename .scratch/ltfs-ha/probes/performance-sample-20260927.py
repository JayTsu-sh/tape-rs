"""显式隔离ltfsd资源采样；不访问设备，不修改业务数据。"""
import json,os,subprocess,time
from pathlib import Path
base=Path('/home/rocky/tape-rs-performance-20260927')
assert os.environ.get('TAPE_RS_PERFORMANCE')=='SR2501L8-isolated'
ps=subprocess.check_output(['pgrep','-x','ltfsd'],text=True).split();assert len(ps)==1
pid=int(ps[0]);proc=Path('/proc')/str(pid);assert os.readlink(proc/'exe')==str(base/'ltfsd')
with (base/f'resources-{pid}.jsonl').open('x') as out:
 while proc.exists():
  status=dict(x.split(':',1) for x in (proc/'status').read_text().splitlines() if ':' in x)
  stat=(proc/'stat').read_text().split();spool=base/'test-data/spool';sizes=[]
  for p in spool.iterdir():
   try:
    if p.is_file():sizes.append(p.stat().st_size)
   except FileNotFoundError:pass
  row=dict(time=time.time(),pid=pid,rss_kib=int(status['VmRSS'].split()[0]),hwm_kib=int(status['VmHWM'].split()[0]),cpu_ticks=int(stat[13])+int(stat[14]),spool_bytes=sum(sizes),spool_files=len(sizes))
  out.write(json.dumps(row)+'\n');out.flush();time.sleep(1)
