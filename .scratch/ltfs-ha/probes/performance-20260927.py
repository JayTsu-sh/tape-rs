"""固定隔离SR池性能基线；记录接收、持久化确认、控制响应、目录规模和内容校验。"""
import concurrent.futures,hashlib,http.client,json,os,statistics,sys,time
from pathlib import Path
assert os.environ.get('TAPE_RS_PERFORMANCE')=='SR2501L8-isolated'
BASE=Path('/home/rocky/tape-rs-performance-20260927');TOP='/sr/performance-20260927-r1'
HOSTS={1:'10.131.9.71',2:'10.131.9.72',3:'10.131.9.74'}
def req(method,path,body=None,host=None,raw=False):
 c=http.client.HTTPConnection(host or HOST,7501,timeout=120);c.request(method,path,body);r=c.getresponse();data=r.read();status=r.status;c.close()
 assert status in (200,201,202),(method,path,status,data[:500])
 return status,data if raw else json.loads(data)
HOST=HOSTS[req('GET','/cluster',host=HOSTS[1])[1]['leader']]
pools=req('GET','/admin/pools')[1]['pools'];assert len(pools)==1 and set(pools[0]['tapes'])=={'SR2501L8','SR2502L8'}
mode=sys.argv[1];assert mode in ('prepare','list','sustained','cold','mixed','multidrive','memory')
manifest=BASE/'manifest.json';items=json.loads(manifest.read_text()) if manifest.exists() else []
def emit(row):
 row['wall_time']=time.time()
 with (BASE/'measurements.jsonl').open('a') as f:f.write(json.dumps(row)+'\n')
 print(json.dumps(row),flush=True)
def payload(path,n):return hashlib.shake_256(path.encode()).digest(n)
def batch(label,count,size,workers=16,create=True):
 directory=TOP+'/'+label
 if create:req('POST','/directories'+directory)
 t=time.monotonic();lat=[]
 def put(i):
  path=f'{directory}/f{i:05}';data=payload(path,size);start=time.monotonic();status,v=req('PUT','/files'+path,data)
  return dict(path=path,size=size,sha256=hashlib.sha256(data).hexdigest(),task=v['task'],started=start,accepted=time.monotonic()-start)
 with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as pool:rows=list(pool.map(put,range(count)))
 accepted=time.monotonic()-t
 def wait(row):
  code,v=req('GET',f"/tasks/{row['task']}?wait=1");assert code in (200,201) and v['status']=='committed',v
  return time.monotonic()-row['started']
 with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as pool:lat=list(pool.map(wait,rows))
 seconds=time.monotonic()-t
 for row in rows:items.append({k:row[k] for k in ('path','size','sha256')})
 manifest.write_text(json.dumps(items))
 emit(dict(kind='write',label=label,count=count,bytes=count*size,accepted_seconds=accepted,committed_seconds=seconds,committed_mib_s=count*size/2**20/seconds,confirmed_files_s=count/seconds,p50_observed_seconds=statistics.median(lat),max_observed_seconds=max(lat),pool=req('GET','/admin/pools')[1]))
def listing(label):
 for directory in (TOP+'/large',TOP+'/small',TOP+'/growth','/sr/perf-20260927/100'):
  samples=[];count=None
  for _ in range(10):
   start=time.monotonic();entries=req('GET','/list?dir='+directory)[1]['entries'];samples.append((time.monotonic()-start)*1000);count=len(entries)
  emit(dict(kind='list',label=label,path=directory,count=count,median_ms=statistics.median(samples),max_ms=max(samples),samples_ms=samples))
if mode=='prepare':
 assert not items;req('POST','/directories'+TOP)
 batch('large',4,8*2**20,1);batch('small',256,4096)
 req('POST','/directories'+TOP+'/growth');listing('before-growth')
 # growth directory created above for comparable baseline
 req('DELETE','/directories'+TOP+'/growth')
 batch('growth',2500,64,32);listing('after-growth')
if mode=='list':listing(sys.argv[2])
if mode=='sustained':
 for i in range(12):batch(f'continuous-{i:02}',32,4096,8)
 # Saturated concurrent staging and oversized admission must leave control requests responsive.
 c=http.client.HTTPConnection(HOST,7501,timeout=15);start=time.monotonic();c.putrequest('PUT','/files'+TOP+'/oversize');c.putheader('Content-Length',str(2**31));c.putheader('Expect','100-continue');c.endheaders();r=c.getresponse();body=r.read();assert r.status==507,(r.status,body);c.close()
 emit(dict(kind='oversize-admission',seconds=time.monotonic()-start,status=507,body=body.decode()))
if mode=='cold':
 start=time.monotonic()
 def read(row):
  code,data=req('GET','/files'+row['path'],raw=True);assert len(data)==row['size'] and hashlib.sha256(data).hexdigest()==row['sha256'];return len(data)
 with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:total=sum(pool.map(read,items))
 emit(dict(kind='cold-http-read',files=len(items),bytes=total,seconds=time.monotonic()-start,verified=True))
if mode=='multidrive':
 batch('switch-volume',1,128*2**20,1,create=len(sys.argv)<3 or sys.argv[2]!='resume')
if mode in ('mixed','multidrive'):
 control=[];read_times=[]
 def reads():
  for row in [x for x in items if '/large/' in x['path']]*3:
   start=time.monotonic();code,data=req('GET','/files'+row['path'],raw=True);assert hashlib.sha256(data).hexdigest()==row['sha256'];read_times.append(time.monotonic()-start)
 def controls():
  for _ in range(60):
   start=time.monotonic();req('GET','/cluster');control.append(time.monotonic()-start);time.sleep(.1)
 with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
  r=pool.submit(reads);c=pool.submit(controls);batch('mixed-two-drives' if mode=='multidrive' else 'mixed',64,32768,16);r.result();c.result()
 emit(dict(kind='mixed',reads=len(read_times),read_p50=statistics.median(read_times),read_max=max(read_times),control_p50=statistics.median(control),control_max=max(control)))

if mode=='memory':
 baseline=len(items);batch('memory-fixed',2500,64,32)
 for i in range(3):
  batch(f'maintenance-{i}',16,64,8)
  start=time.monotonic();_,result=req('POST','/sync')
  emit(dict(kind='full-maintenance',cycle=i,seconds=time.monotonic()-start,response=result))
 # 新增内存压力样本：全部核对已提交摘要，分层抽取每第25个文件作真实冷读。
 extra=items[baseline:];sampled=0
 for n,row in enumerate(extra):
  _,stat=req('GET','/stat'+row['path'])
  assert stat.get('current',stat)['sha256']==row['sha256'],stat
  if n%25==0:
   _,data=req('GET','/files'+row['path'],raw=True)
   assert hashlib.sha256(data).hexdigest()==row['sha256'];sampled+=1
 emit(dict(kind='memory-verification',committed_hashes=len(extra),cold_read_sample=sampled,passed=True))
