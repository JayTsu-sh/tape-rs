"""LISA 第二驱动器上的直接库基线；EE节点2已正常下线，LE已退出。"""
import json,os,subprocess,time
from pathlib import Path
assert os.environ.get('TAPE_RS_PERF_COMPARE')=='PF2701L08-PF2702L08'
base=Path('/home/rocky/tape-rs-le-compare-20260927');cli='/home/rocky/tape-rs-performance-20260927/tape-rs'
assert subprocess.run(['pgrep','-x','ltfs'],capture_output=True).returncode==1
assert not os.path.ismount('/ltfs')
changer,drive='/dev/sg9','/dev/sg7';serial='IBMlisa42299'
assert serial in subprocess.check_output(['sg_inq','--page=0x80',drive],text=True)
assert 'IBMlisa41D6C_LL3' in subprocess.check_output(['sg_inq','--page=0x80',changer],text=True)
slots={'PF2701L08':11,'PF2702L08':12};loaded=None
log=(base/'native-measurements.jsonl').open('x')
def emit(row):
 row['wall_time']=time.time();s=json.dumps(row);log.write(s+'\n');log.flush();print(s,flush=True)
def command(*args):return subprocess.check_output([cli,*args],text=True)
def inventory():return command('inventory','--device',changer,'--no-drive-scan')
def unload():
 global loaded
 if loaded is None:return
 assert f'驱动器   2 [载带]: {loaded}' in inventory()
 start=time.monotonic();command('drive-unload','--device',drive);unthread=time.monotonic()-start
 command('unload','--device',changer,'--drive','1','--slot',str(slots[loaded]));total=time.monotonic()-start
 emit(dict(interface='rust-library',kind='unload',barcode=loaded,unthread_seconds=unthread,total_seconds=total));loaded=None
 return total
def load(barcode):
 global loaded
 assert loaded is None
 inv=inventory();assert '驱动器   2 [空]' in inv and f'存储槽  {slots[barcode]} [载带]: {barcode}' in inv
 start=time.monotonic();command('load','--device',changer,'--drive','1','--slot',str(slots[barcode]));move=time.monotonic()-start
 command('drive-load','--device',drive);total=time.monotonic()-start;loaded=barcode
 emit(dict(interface='rust-library',kind='load',barcode=barcode,move_seconds=move,total_seconds=total));return total
def bench(mode,group='all',path=None,once=False):
 assert loaded is not None
 args=[str(base/'performance_compare'),'--device',drive,'--manifest',str(base/(loaded+'.json')),'--source',str(base/'source'),'--group',group,'--mode',mode]
 if path:args+=['--path',path]
 if once:args+=['--once']
 start=time.monotonic();process=subprocess.Popen(args,stdout=subprocess.PIPE,text=True)
 out=process.stdout.read();process.stdout.close()
 pid,status,usage=os.wait4(process.pid,0);process.returncode=os.waitstatus_to_exitcode(status)
 assert pid==process.pid and process.returncode==0,(pid,process.returncode,out)
 emit(dict(interface='rust-library',kind='resource',mode=mode,group=group,maxrss_kib=usage.ru_maxrss,user_seconds=usage.ru_utime,system_seconds=usage.ru_stime,wall_seconds=time.monotonic()-start))
 for line in out.splitlines():emit(json.loads(line))
 return time.monotonic()-start
load('PF2701L08')
for group in ['large','small','growth']+[f'continuous-{i:02}' for i in range(12)]+['mixed']:
 bench('write',group)
 if group in ('small','growth'):
  for directory in ('large','small'):bench('list',directory)
unload();load('PF2701L08');bench('read');unload();load('PF2702L08')
for group in ('switch-volume','switch-read'):bench('write',group)
unload();load('PF2702L08');bench('read')
for i in range(6):
 target='PF2701L08' if i%2==0 else 'PF2702L08';path='comparison/large/f00000' if i%2==0 else 'comparison/switch-read/f00000'
 start=time.monotonic();unload();load(target);bench('read',path=path,once=True)
 emit(dict(interface='rust-library',kind='cross-tape-first-read',cycle=i,barcode=target,bytes=8*2**20,seconds=time.monotonic()-start))
unload();emit(dict(kind='complete',passed=True))
