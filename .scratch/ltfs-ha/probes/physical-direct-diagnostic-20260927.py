"""计时轮结束后的独立8MiB写诊断：记录LE实际SG direct结果，之后重建Rust空卷基线。"""
import hashlib,json,os,signal,subprocess,time
from pathlib import Path
assert os.environ.get('TAPE_RS_PERF_COMPARE')=='RC0018L9-RC0017L9'
b=Path('/root/tape-rs-io-20260927');root=b/'le-mount';barcode='RC0018L9';serial='11EB4A80F1'
lead='/opt/ibm/ltfsle/bin/leadm'
def command(*args):return subprocess.check_output([lead,*args,'-s','localhost:17600'],text=True)
assert Path('/sys/module/sg/parameters/allow_dio').read_text().strip()=='1'
assert json.loads(command('tape','show',barcode))['slot_type']=='SLOT'
command('tape','move','-L','drive','-d',serial,barcode);command('tape','mount','-d',serial,barcode)
pid=int((b/'le.pid').read_text());assert str(root).encode() in Path(f'/proc/{pid}/cmdline').read_bytes()
trace_path=b/'physical-le-direct.trace';assert not trace_path.exists()
trace=subprocess.Popen(['strace','-f','-ttt','-T','-s','32','-xx','-e','trace=ioctl','-p',str(pid),'-o',str(trace_path)],stdout=subprocess.DEVNULL,stderr=subprocess.PIPE,start_new_session=True)
phases=[]
try:
 for _ in range(100):
  assert trace.poll() is None,'tracer exited before attach'
  fields=dict(line.split(':',1) for line in Path(f'/proc/{pid}/status').read_text().splitlines() if ':' in line)
  if int(fields['TracerPid'])==trace.pid:break
  time.sleep(.1)
 else:raise RuntimeError('tracer did not attach')
 path=root/barcode/'direct-diagnostic-20260927/data';path.parent.mkdir(exist_ok=False)
 data=os.urandom(8*2**20)
 phases.append({'phase':'write','time':time.time()})
 with path.open('xb',buffering=0) as f:
  assert f.write(data)==len(data)
  phases.append({'phase':'fdatasync','time':time.time()});os.fdatasync(f.fileno())
  phases.append({'phase':'fsync','time':time.time()});os.fsync(f.fileno())
 phases.append({'phase':'sync','time':time.time()});os.setxattr(root/barcode,'user.ltfs.sync',b'1')
 phases.append({'phase':'done','time':time.time()})
 assert hashlib.sha256(path.read_bytes()).digest()==hashlib.sha256(data).digest()
finally:
 if trace.poll() is None:trace.send_signal(signal.SIGINT)
 _,err=trace.communicate(timeout=30)
 (b/'physical-le-direct-tracer.log').write_bytes(err)
 (b/'physical-le-direct-phases.json').write_text(json.dumps(phases,indent=2))
command('tape','move','-L','homeslot',barcode)
print('independent LE direct diagnostic complete',flush=True)
