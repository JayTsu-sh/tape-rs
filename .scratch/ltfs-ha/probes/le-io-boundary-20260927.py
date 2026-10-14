"""专用 PF 卷的 LE I/O 命令诊断；trace 数据不用于性能计时。"""
import hashlib,json,os,pathlib,signal,subprocess,time
assert os.environ.get('TAPE_RS_PERF_COMPARE')=='PF2701L08-PF2702L08'
base=pathlib.Path('/home/rocky/tape-rs-le-compare-20260927');leadm='/opt/ibm/ltfsle/bin/leadm';barcode='PF2701L08'
def command(*args):return subprocess.check_output([leadm,*args],text=True)
assert 'IBMlisa42299' in command('drive','list')
s=json.loads(command('tape','show',barcode));assert s['slot_type']=='SLOT' and s['assignment']=='UNASSIGNED'
command('tape','assign',barcode);command('tape','move','-L','drive','-d','IBMlisa42299',barcode);command('tape','mount','-d','IBMlisa42299',barcode)
root=pathlib.Path('/ltfs')/barcode;target=root/'io-boundary-20260927';assert not target.exists();assert os.path.ismount('/ltfs')
pids=subprocess.check_output(['pgrep','-x','ltfs'],text=True).split();assert len(pids)==1
log=base/'le-io-boundary-trace';assert not log.exists();err=(base/'le-io-boundary-strace.log').open('x')
p=subprocess.Popen([str(base/'strace'),'-f','-ttt','-T','-s','32','-xx','-e','trace=ioctl','-o',str(log),'-p',pids[0]],stdout=subprocess.DEVNULL,stderr=err)
records=[]
def mark(phase):
 row=dict(phase=phase,time=time.time());records.append(row);print(json.dumps(row),flush=True)
try:
 time.sleep(.5);assert p.poll() is None
 mark('mkdir');target.mkdir()
 payload=hashlib.shake_256(b'LE IO command diagnostic 20260927').digest(8*2**20)
 mark('open');f=(target/'data').open('xb',buffering=0)
 mark('write');assert f.write(payload)==len(payload)
 mark('fdatasync');os.fdatasync(f.fileno())
 mark('fsync');os.fsync(f.fileno())
 mark('close');f.close()
 mark('sync');os.setxattr(root,'user.ltfs.sync',b'1')
 mark('read-first');assert (target/'data').read_bytes()==payload
 mark('read-repeat');assert (target/'data').read_bytes()==payload
 mark('unload');command('tape','move','-L','homeslot',barcode)
 mark('unassign');command('tape','unassign',barcode)
 mark('complete')
finally:
 p.send_signal(signal.SIGINT);p.wait(timeout=10);err.close()
 (base/'le-io-boundary-phases.json').write_text(json.dumps(records,indent=2))
