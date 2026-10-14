"""隔离 SR 池单驱动器：跨带读的检查点之前暂停旧节点，接管后恢复。"""
import concurrent.futures
import json
import os
from pathlib import Path
import signal
import subprocess
import time
import urllib.error
import urllib.request

BASE = Path('/home/rocky/tape-rs-namespace-20260927')
assert os.environ.get('TAPE_RS_NAMESPACE') == 'SR2501L8-SR2502L8'
def query(host, path):
    with urllib.request.urlopen('http://' + host + ':7501' + path, timeout=8) as response:
        return json.load(response)
pids = subprocess.check_output(['pgrep', '-x', 'ltfsd'], text=True).split()
assert len(pids) == 1
pid = int(pids[0])
assert os.readlink(f'/proc/{pid}/exe') == str(BASE/'ltfsd')
args = Path(f'/proc/{pid}/cmdline').read_bytes().split(bytes([0]))
assert args.count(b'--drive-serial') == 1 and b'IBMtaper2287' in args
parent = int(next(l for l in Path(f'/proc/{pid}/status').read_text().splitlines() if l.startswith('PPid:')).split()[1])
parent_args = Path(f'/proc/{parent}/cmdline').read_bytes().split(bytes([0]))
assert str(BASE/'ltfsd').encode() in parent_args
def resume():
    os.kill(pid, signal.SIGCONT)
    # sudo follows its child's group stop; resume it too so it can reap on shutdown.
    os.kill(parent, signal.SIGCONT)
st = query('127.0.0.1', '/cluster')
assert st['role'] == 'Leader' and st['serving_round'] == st['executor']['round']
assert 'SR2502L8' in st['local']
pools = query('127.0.0.1', '/admin/pools')['pools']
assert len(pools) == 1 and set(pools[0]['tapes']) == {'SR2501L8','SR2502L8'}
assert pools[0]['uuid'] == '51cfd06c-bf79-4bbf-ac85-886608a0ab4b'
tids = [int(p.name) for p in Path(f'/proc/{pid}/task').iterdir() if (p/'comm').read_text().strip() == 'ltfsd-exec']
assert len(tids) == 1
trace = BASE/'checkpoint.trace'
assert not trace.exists()
def read_old():
    try:
        with urllib.request.urlopen('http://127.0.0.1:7501/files/sr/seed', timeout=55) as r:
            return r.status, r.read().decode()
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()
with (BASE/'checkpoint-strace.log').open('w') as log:
    tracer = subprocess.Popen([str(BASE/'strace'),'-p',str(tids[0]),'-e','trace=ioctl','-e','inject=ioctl:delay_enter=500ms','-v','-xx','-s','96','-tt','-T','-o',str(trace)],stdout=log,stderr=log)
    stopped = False
    try:
        deadline = time.monotonic()+10
        while f'TracerPid:\t{tracer.pid}' not in Path(f'/proc/{pid}/task/{tids[0]}/status').read_text():
            assert time.monotonic()<deadline
            time.sleep(.02)
        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
            future = pool.submit(read_old)
            deadline = time.monotonic()+25
            while True:
                text = trace.read_text()
                inventories = [l for l in text.splitlines() if 'cmdp="\\xb8' in l and ') = 0' in l and 'status=0,' in l]
                if len(inventories) == 2:
                    os.kill(pid, signal.SIGSTOP)
                    stopped = True
                    break
                assert len(inventories)<2 and time.monotonic()<deadline and not future.done(), text[-1500:]
                time.sleep(.005)
            deadline = time.monotonic()+25
            takeover = None
            while takeover is None:
                for host in ('10.131.9.71','10.131.9.72','10.131.9.74'):
                    # The paused old node cannot answer HTTP.
                    if int(host.rsplit('.',1)[1]) == {1:71,2:72,3:74}[st['id']]:
                        continue
                    candidate = query(host, '/cluster')
                    if candidate['serving_round'] and candidate['serving_round']>st['serving_round']:
                        takeover = candidate
                        break
                assert time.monotonic()<deadline
                if takeover is None:time.sleep(.1)
            resume()
            stopped = False
            result = future.result(timeout=15)
        assert result[0] == 503, result
        assert '写入带检查点失败' in result[1], result
        assert '执行资格已失去' in result[1] or 'reservation conflict' in result[1] or 'sense_key=0x06' in result[1], result
        evidence = dict(old=st,takeover=takeover,response_status=result[0],response_body=result[1],trace=trace.read_text())
        (BASE/'checkpoint.event.json').write_text(json.dumps(evidence,ensure_ascii=False,indent=2))
        print('PASS checkpoint ownership lost: old HTTP 503, newer executor serving',flush=True)
    finally:
        if stopped:resume()
        if tracer.poll() is None:tracer.terminate()
        tracer.wait(timeout=10)
