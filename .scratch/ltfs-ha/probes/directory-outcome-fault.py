"""隔离 SR 池目录删除：批次挂载中暂停旧执行者，接管后恢复并核验未定结果。"""
import concurrent.futures
import json
import os
from pathlib import Path
import signal
import subprocess
import time
import urllib.error
import urllib.request

BASE=Path('/home/rocky/tape-rs-directory-outcome-20260927')
assert os.environ.get('TAPE_RS_DIRECTORY_OUTCOME')=='SR2501L8-isolated'
PATH='/sr/directory-outcome-20260927'
HOSTS={1:'10.131.9.71',2:'10.131.9.72',3:'10.131.9.74'}
def req(host,method,path):
    try:
        with urllib.request.urlopen(urllib.request.Request('http://'+host+':7501'+path,method=method),timeout=55) as r:return r.status,json.load(r)
    except urllib.error.HTTPError as e:return e.code,json.load(e)
ps=subprocess.check_output(['pgrep','-x','ltfsd'],text=True).split();assert len(ps)==1
pid=int(ps[0]);assert os.readlink(f'/proc/{pid}/exe')==str(BASE/'ltfsd')
st=req('127.0.0.1','GET','/cluster')[1]
assert st['role']=='Leader' and st['serving_round']==st['executor']['round']
pools=req('127.0.0.1','GET','/admin/pools')[1]['pools'];assert len(pools)==1 and set(pools[0]['tapes'])=={'SR2501L8','SR2502L8'}
assert req('127.0.0.1','POST','/directories'+PATH)[0]==201
parent=int(next(x for x in Path(f'/proc/{pid}/status').read_text().splitlines() if x.startswith('PPid:')).split()[1])
assert str(BASE/'ltfsd').encode() in Path(f'/proc/{parent}/cmdline').read_bytes()
tids=[int(p.name) for p in Path(f'/proc/{pid}/task').iterdir() if (p/'comm').read_text().strip()=='ltfsd-exec'];assert len(tids)==1
trace=BASE/'directory.trace';assert not trace.exists()
def resume():
    os.kill(pid,signal.SIGCONT);os.kill(parent,signal.SIGCONT)
with (BASE/'strace.log').open('w') as log:
    tracer=subprocess.Popen(['/home/rocky/tape-rs-namespace-20260927/strace','-p',str(tids[0]),'-e','trace=ioctl','-e','inject=ioctl:delay_enter=500ms','-v','-xx','-s','96','-tt','-T','-o',str(trace)],stdout=log,stderr=log)
    stopped=False
    try:
        deadline=time.monotonic()+10
        while f'TracerPid:\t{tracer.pid}' not in Path(f'/proc/{pid}/task/{tids[0]}/status').read_text():
            assert time.monotonic()<deadline;time.sleep(.02)
        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
            future=pool.submit(req,'127.0.0.1','DELETE','/directories'+PATH)
            deadline=time.monotonic()+30
            while True:
                text=trace.read_text()
                reads=[x for x in text.splitlines() if 'cmdp="\\x08' in x and ') = 0' in x and 'status=0,' in x]
                if reads:
                    os.kill(pid,signal.SIGSTOP);stopped=True;break
                assert time.monotonic()<deadline and not future.done(),text[-1800:]
                time.sleep(.005)
            deadline=time.monotonic()+25;takeover=None
            while takeover is None:
                for node,host in HOSTS.items():
                    if node==st['id']:continue
                    candidate=req(host,'GET','/cluster')[1]
                    if candidate['serving_round'] and candidate['serving_round']>st['serving_round']:takeover=candidate;break
                assert time.monotonic()<deadline
                if takeover is None:time.sleep(.1)
            resume();stopped=False
            response=future.result(timeout=20)
        (BASE/'response.json').write_text(json.dumps(dict(old=st,takeover=takeover,response=response),ensure_ascii=False,indent=2))
        assert response[0]==500 and response[1]['status']=='indeterminate',response
        assert '0x2a' in response[1]['detail'] or '预留' in response[1]['detail'] or 'reservation' in response[1]['detail'],response
        successor=HOSTS[takeover['id']]
        stat=req(successor,'GET','/stat'+PATH)
        assert stat[0]==200,stat
        assert req(successor,'DELETE','/directories'+PATH)[0]==201
        assert req(successor,'POST','/directories'+PATH)[0]==201
        assert req(successor,'DELETE','/directories'+PATH)[0]==201
        result=dict(old=st,takeover=takeover,response=response,recovered=stat,passed=True)
        (BASE/'fault.json').write_text(json.dumps(result,ensure_ascii=False,indent=2))
        print(json.dumps(result,ensure_ascii=False),flush=True)
    finally:
        if stopped:resume()
        if tracer.poll() is None:tracer.terminate()
        tracer.wait(timeout=10)
