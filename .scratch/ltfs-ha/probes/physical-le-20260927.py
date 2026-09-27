"""与直接库共用清单，LE FUSE 串行数据操作 + 每批显式 LE sync（DP 索引；卸载时更新 IP）；含换带。"""
import hashlib,json,os,subprocess,time,resource
from pathlib import Path
assert os.environ.get('TAPE_RS_PERF_COMPARE')=='RC0018L9-RC0017L9'
base=Path('/root/tape-rs-io-20260927');root=base/'le-mount';leadm='/opt/ibm/ltfsle/bin/leadm';serial='11EB4A80F1'
assert os.path.ismount(root)
assert serial in subprocess.check_output([leadm,'drive','list','-s','localhost:17600'],text=True)
items={b:json.loads((base/(b+'.json')).read_text())['files'] for b in ('RC0018L9','RC0017L9')}
log=(base/'le-measurements.jsonl').open('x');loaded=None
ltfs_pids=subprocess.check_output(['pgrep','-x','ltfs'],text=True).split();assert len(ltfs_pids)==1
ltfs_proc=Path('/proc')/ltfs_pids[0]

def emit(row):
 status=dict(x.split(':',1) for x in (ltfs_proc/'status').read_text().splitlines() if ':' in x)
 stat=(ltfs_proc/'stat').read_text().split();own=resource.getrusage(resource.RUSAGE_SELF);children=resource.getrusage(resource.RUSAGE_CHILDREN)
 row.update(ltfs_rss_kib=int(status['VmRSS'].split()[0]),ltfs_hwm_kib=int(status['VmHWM'].split()[0]),ltfs_cpu_seconds=(int(stat[13])+int(stat[14]))/os.sysconf('SC_CLK_TCK'),probe_hwm_kib=own.ru_maxrss,probe_cpu_seconds=own.ru_utime+own.ru_stime+children.ru_utime+children.ru_stime)
 row['scsi_io']={n:(Path('/sys/class/scsi_device/16:0:0:0/device')/n).read_text().strip() for n in ('iorequest_cnt','iodone_cnt','ioerr_cnt')};row['wall_time']=time.time();s=json.dumps(row);log.write(s+'\n');log.flush();print(s,flush=True)
def command(*args):return subprocess.check_output([leadm,*args,'-s','localhost:17600'],text=True)
def unload():
 global loaded
 if loaded is None:return
 start=time.monotonic();command('tape','move','-L','homeslot',loaded);total=time.monotonic()-start
 emit(dict(interface='IBM-LE-FUSE',kind='unload',barcode=loaded,total_seconds=total));loaded=None
 return total
def load(barcode):
 global loaded
 assert loaded is None
 show=json.loads(command('tape','show',barcode));assert show['slot_type']=='SLOT',show
 start=time.monotonic();command('tape','move','-L','drive','-d',serial,barcode);move=time.monotonic()-start
 command('tape','mount','-d',serial,barcode);total=time.monotonic()-start;loaded=barcode
 # 目录遍历让 LE 建立本卷命名空间；单列，不能把索引缓存命中算作实际读带。
 start=time.monotonic();list(os.scandir(root/barcode));namespace=time.monotonic()-start
 emit(dict(interface='IBM-LE-FUSE',kind='load',barcode=barcode,move_seconds=move,total_seconds=total,namespace_seconds=namespace));return total+namespace

def read(row):
 h=hashlib.sha256();size=0
 with (root/loaded/row['path']).open('rb',buffering=0) as f:
  while True:
   data=f.read(2**20)
   if not data:break
   h.update(data);size+=len(data)
 assert size==row['size'] and h.hexdigest()==row['sha256'],row

def write(group):
 rows=[r for r in items[loaded] if r['group']==group];assert rows
 for row in rows:
  assert not (root/loaded/row['path']).exists()
  h=hashlib.sha256()
  with (base/'source'/row['path']).open('rb') as source:
   for data in iter(lambda:source.read(64*1024),b''):h.update(data)
  assert h.hexdigest()==row['sha256']
 start=time.monotonic();mixed_reads=0
 for i,row in enumerate(rows):
  target=root/loaded/row['path'];target.parent.mkdir(parents=True,exist_ok=True)
  with (base/'source'/row['path']).open('rb',buffering=0) as source,target.open('xb',buffering=0) as dest:
   while True:
    data=source.read(2**20)
    if not data:break
    assert dest.write(data)==len(data)
  if group=='mixed' and (i+1)%16==0:
   large=[r for r in items[loaded] if r['group']=='large'];assert len(large)==4
   for _ in range(3):read(large[mixed_reads%4]);mixed_reads+=1
 data_seconds=time.monotonic()-start
 commit=time.monotonic();os.setxattr(root/loaded,'user.ltfs.sync',b'1');commit_seconds=time.monotonic()-commit
 emit(dict(interface='IBM-LE-FUSE',kind='write',group=group,count=len(rows),bytes=sum(r['size'] for r in rows),mixed_reads=mixed_reads,data_seconds=data_seconds,commit_seconds=commit_seconds,total_seconds=time.monotonic()-start))
def reads(path=None):
 rows=[r for r in items[loaded] if path is None or r['path']==path];assert rows
 for label in (('first',) if path else ('first','repeat')):
  start=time.monotonic()
  for row in rows:read(row)
  emit(dict(interface='IBM-LE-FUSE',kind='read',label=label,count=len(rows),bytes=sum(r['size'] for r in rows),seconds=time.monotonic()-start,verified=True))
def listing(group):
 samples=[];count=0
 for _ in range(10):
  start=time.monotonic();entries=list(os.scandir(root/loaded/'comparison'/group));count=len(entries);samples.append((time.monotonic()-start)*1000)
 emit(dict(interface='IBM-LE-FUSE',kind='list',group=group,count=count,samples_ms=samples))

load('RC0018L9')
for group in ['large','stream','small','growth']+[f'continuous-{i:02}' for i in range(12)]:
 write(group)
 if group in ('small','growth'):
  for directory in ('large','small'):listing(directory)
unload()
# 取消分配并重新分配，清除该卷的 FUSE 命名空间后再做首次读。
command('tape','unassign','RC0018L9');command('tape','assign','RC0018L9');load('RC0018L9');reads();unload()
load('RC0017L9')
for group in ('switch-volume','switch-read'):write(group)
unload();command('tape','unassign','RC0017L9');command('tape','assign','RC0017L9');load('RC0017L9');reads()
for i in range(2):
 target='RC0018L9' if i%2==0 else 'RC0017L9';path='comparison/large/f00000' if i%2==0 else 'comparison/switch-read/f00000'
 start=time.monotonic();unload();load(target);reads(path)
 emit(dict(interface='IBM-LE-FUSE',kind='cross-tape-first-read',cycle=i,barcode=target,bytes=256*2**20,seconds=time.monotonic()-start))
unload();emit(dict(kind='complete',passed=True))

# 独立诊断不写入计时JSONL；发生错误则外层不会发布LE完成标记。
subprocess.run(['python3',str(base/'physical-direct-diagnostic-20260927.py')],check=True)
