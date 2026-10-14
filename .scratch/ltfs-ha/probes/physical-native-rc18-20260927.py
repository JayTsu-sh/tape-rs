"""实机单驱动器直接库基线；私有 LE 已正常退出，两盘专用 RC 介质已重建空卷基线。"""
import json,os,subprocess,time
from pathlib import Path
assert os.environ.get('TAPE_RS_PERF_COMPARE')=='RC0018L9-RC0017L9'
base=Path('/root/tape-rs-io-20260927');cli='/root/tape-rs-io-20260927/tape-rs-candidate'
assert Path('/sys/module/sg/parameters/allow_dio').read_text().strip()=='0'
assert subprocess.run(['pgrep','-x','ltfs'],capture_output=True).returncode==1
assert not os.path.ismount(str(base/'le-mount'))
changer,drive='/dev/sg5','/dev/sg3';serial='11EB4A80F1'
assert serial in subprocess.check_output(['sg_inq','--page=0x80',drive],text=True)
assert '55L3A7802K19LL01' in subprocess.check_output(['sg_inq','--page=0x80',changer],text=True)
slots={'RC0018L9':7,'RC0017L9':8};loaded=None
pending_checkpoint=0.0
log=(base/'native-rc18-measurements.jsonl').open('x')
def emit(row):
 row['scsi_io']={n:(Path('/sys/class/scsi_generic/sg3/device')/n).read_text().strip() for n in ('iorequest_cnt','iodone_cnt','ioerr_cnt')};row['wall_time']=time.time();s=json.dumps(row);log.write(s+'\n');log.flush();print(s,flush=True)
def command(*args):return subprocess.check_output([cli,*args],text=True)
def inventory():return command('inventory','--device',changer,'--no-drive-scan')
def unload():
 global loaded,pending_checkpoint
 if loaded is None:return
 assert f'驱动器   1 [载带]: {loaded}' in inventory()
 start=time.monotonic();checkpoint=pending_checkpoint;pending_checkpoint=0.0
 unthread_start=time.monotonic();command('drive-unload','--device',drive);unthread=time.monotonic()-unthread_start
 command('unload','--device',changer,'--drive','0','--slot',str(slots[loaded]));total=time.monotonic()-start+checkpoint
 emit(dict(interface='rust-library',kind='unload',barcode=loaded,checkpoint_seconds=checkpoint,unthread_seconds=unthread,total_seconds=total));loaded=None
 return total
def load(barcode):
 global loaded
 assert loaded is None
 inv=inventory();assert '驱动器   1 [空]' in inv and f'存储槽 {slots[barcode]:3} [载带]: {barcode}' in inv
 start=time.monotonic();command('load','--device',changer,'--drive','0','--slot',str(slots[barcode]));move=time.monotonic()-start
 command('drive-load','--device',drive);total=time.monotonic()-start;loaded=barcode
 emit(dict(interface='rust-library',kind='load',barcode=barcode,move_seconds=move,total_seconds=total));return total
def bench(mode,group='all',path=None,once=False):
 global pending_checkpoint
 assert loaded is not None
 args=[str(base/'performance_compare'),'--physical','--device',drive,'--manifest',str(base/(loaded+'.json')),'--source',str(base/'source'),'--group',group,'--mode',mode]
 if path:args+=['--path',path]
 if once:args+=['--once']
 start=time.monotonic();process=subprocess.Popen(args,stdout=subprocess.PIPE,text=True)
 out=''
 for line in process.stdout:
  out+=line;row=json.loads(line);emit(row)
  if row.get('kind')=='checkpoint':pending_checkpoint=row['seconds']
 process.stdout.close()
 pid,status,usage=os.wait4(process.pid,0);process.returncode=os.waitstatus_to_exitcode(status)
 assert pid==process.pid and process.returncode==0,(pid,process.returncode,out)
 emit(dict(interface='rust-library',kind='resource',mode=mode,group=group,maxrss_kib=usage.ru_maxrss,user_seconds=usage.ru_utime,system_seconds=usage.ru_stime,wall_seconds=time.monotonic()-start))
 return time.monotonic()-start
load('RC0018L9')
bench('write-groups')
unload();load('RC0018L9');bench('read');unload()
emit(dict(kind='complete',passed=True,scope='RC18 only; reload measured, cross-tape swap deferred'))
