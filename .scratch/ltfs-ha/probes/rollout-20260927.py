"""本轮正式部署：显式路径、SHA、停止条件和闭库备份；不恢复旧状态。"""
import hashlib,json,pathlib,subprocess,sys
ROOT=pathlib.Path('/work/jay/tape-rs-ltfs-recovery-ibm-interop')
BIN=pathlib.Path('/work/cargo-target/x86_64-unknown-linux-gnu/release')
BASE='/home/rocky/tape-rs-rollout-20260927'
OLD='/home/rocky/tape-rs-symlink-reclaim-prod-20260927'
ISOLATED='/home/rocky/tape-rs-performance-20260927'
START='/home/rocky/tape-rs-ltfs25-20260924/start.sh'
HOSTS=['71','72','74']
HASH={'ltfsd':'ef653655aef6453fbc74a195f3524b3c212b5b51cfa1010d51319b938588d201','tape-fuse':'0b9599d76a55464ee37e9e4f1f5e7e1e860b9dea9d72be3894927d2c7cd30ed6','ltfsctl':'aa74e826347d17696c35f80756b28bb97541ea8aee6df2706787b5b87447fb6a'}
def remote(h,code,timeout=55):
 p=subprocess.run(['ssh','-o','BatchMode=yes','-o','ConnectTimeout=8','rocky@10.131.9.'+h,'sudo python3 -'],input=code,text=True,capture_output=True,timeout=timeout)
 if p.returncode:raise RuntimeError((h,p.stdout,p.stderr))
 return p.stdout
PRE=f'''import os,sys,pathlib,json,subprocess,hashlib,sqlite3,time,shutil,signal,urllib.request
BASE={BASE!r};OLD={OLD!r};ISOLATED={ISOLATED!r};START={START!r};HASH={HASH!r}
def pids():
 p=subprocess.run(['pgrep','-x','ltfsd'],capture_output=True,text=True);assert p.returncode in (0,1);return list(map(int,p.stdout.split()))
def query(path):
 with urllib.request.urlopen('http://127.0.0.1:7401'+path,timeout=10) as r:return json.load(r)
'''
if __name__=='__main__':
 mode=sys.argv[1]
 if mode=='stage':
  for h in HOSTS+['73']:
   print(h,remote(h,PRE+f'''
assert not pathlib.Path(BASE).exists()
for pid in pids():assert os.readlink(f'/proc/{{pid}}/exe')==ISOLATED+'/ltfsd'
pathlib.Path(BASE).mkdir();os.chown(BASE,1000,1000)
os.umask(0o077)
if {h!r}!='73':
 assert OLD+'/ltfsd' in pathlib.Path(START).read_text()
 shutil.copy2(START,BASE+'/start.before.sh')
 subprocess.run(['tar','-czf',BASE+'/pre-upgrade.tgz','-C','/home/rocky','ltfsd-data',OLD.removeprefix('/home/rocky/'),START.removeprefix('/home/rocky/')],check=True)
else:
 assert subprocess.run(['pgrep','-x','tape-fuse'],capture_output=True).returncode in (0,1)
 for name in ('cache','mnt'):pathlib.Path(BASE+'/'+name).mkdir()
 shutil.copy2(OLD+'/expected.json',BASE+'/expected.json')
 s=pathlib.Path(OLD+'/probe.py').read_text().replace(OLD,BASE)
 pathlib.Path(BASE+'/probe.py').write_text(s)
print('staged backup')
'''))
   for name in (['ltfsd'] if h!='73' else ['tape-fuse','ltfsctl']):
    assert hashlib.sha256((BIN/name).read_bytes()).hexdigest()==HASH[name]
    subprocess.run(['scp','-q',str(BIN/name),f'rocky@10.131.9.{h}:{BASE}/{name}'],check=True)
    print(remote(h,PRE+f"assert hashlib.sha256(pathlib.Path(BASE+'/{name}').read_bytes()).hexdigest()==HASH['{name}']\nprint('verified {name}')\n"))
 elif mode=='activate':
  for h in HOSTS:
   print(h,remote(h,PRE+'''
assert not pids()
assert pathlib.Path(BASE+'/pre-upgrade.tgz').stat().st_size>0
assert hashlib.sha256(pathlib.Path(BASE+'/ltfsd').read_bytes()).hexdigest()==HASH['ltfsd']
s=pathlib.Path(START).read_text();assert OLD+'/ltfsd' in s
s=s.replace(OLD+'/ltfsd',BASE+'/ltfsd')
pathlib.Path(BASE+'/start.sh').write_text(s)
subprocess.run(['bash','-n',BASE+'/start.sh'],check=True)
shutil.copy2(BASE+'/start.sh',START)
print('activated startup path; process still stopped')
'''))
 elif mode=='start':
  for h in sys.argv[2:] or HOSTS:
   n=HOSTS.index(h)+1
   print(h,remote(h,PRE+f'''
assert not pids()
assert BASE+'/ltfsd' in pathlib.Path(START).read_text()
log=open(BASE+'/ltfsd.log','ab',buffering=0)
p=subprocess.Popen(['bash',START,'{n}'],stdin=subprocess.DEVNULL,stdout=log,stderr=log,start_new_session=True)
print(p.pid)
'''))
 elif mode=='snapshot':
  data={}
  for h in HOSTS:
   data[h]=json.loads(remote(h,PRE+'''
ps=pids();assert len(ps)==1
assert os.readlink(f'/proc/{ps[0]}/exe')==BASE+'/ltfsd'
assert hashlib.sha256(pathlib.Path(f'/proc/{ps[0]}/exe').read_bytes()).hexdigest()==HASH['ltfsd']
db=sqlite3.connect('file:/home/rocky/ltfsd-data/directory.db?mode=ro',uri=True);db.row_factory=sqlite3.Row
rows=[dict(r) for r in db.execute('select * from files order by pool_uuid,path')]
print(json.dumps(dict(cluster=query('/cluster'),pools=query('/admin/pools'),files=rows)))
'''))
  assert all(d['files']==data['71']['files'] for d in data.values())
  path=ROOT/'.scratch/ltfs-ha/probes/results'/('rollout-20260927-'+sys.argv[2]+'.json');assert not path.exists();path.write_text(json.dumps(data,ensure_ascii=False,indent=2)+'\n')
  print(json.dumps({h:dict(cluster=d['cluster'],rows=len(d['files'])) for h,d in data.items()},ensure_ascii=False))
 else:raise ValueError(mode)
