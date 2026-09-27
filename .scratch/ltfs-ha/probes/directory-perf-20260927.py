"""固定 100/1000 文件目录，统计真实 FUSE 枚举耗时及 HTTP 请求，非物理带速度。"""
import collections
import concurrent.futures
import hashlib
import http.client
import http.server
import json
import os
from pathlib import Path
import subprocess
import sys
import threading
import time

assert os.environ.get('TAPE_RS_DIRECTORY_PERF') == 'SR2501L8-isolated'
base=Path('/home/rocky/tape-rs-directory-perf-20260927')
root=base/'mnt'
mode=sys.argv[1]
c=http.client.HTTPConnection('10.131.9.71',7501,timeout=10);c.request('GET','/cluster');cluster=json.loads(c.getresponse().read());c.close()
host={1:'10.131.9.71',2:'10.131.9.72',3:'10.131.9.74'}[cluster['leader']]
def req(method,path,data=None):
    c=http.client.HTTPConnection(host,7501,timeout=120)
    c.request(method,path,data)
    r=c.getresponse(); body=r.read(); status=r.status;c.close()
    assert status in (200,201,202),(status,body)
    return json.loads(body)
if mode=='prepare':
    assert os.path.ismount(root)
    top=root/'sr/perf-20260927';top.mkdir(exist_ok=True)
    for count in (100,1000):
        d=top/str(count);d.mkdir(exist_ok=True)
        for name in ('empty','subdir'): (d/name).mkdir(exist_ok=True)
        for name,target in [('link','f00000'),('dangling','absent'),('loop','loop')]:
            if os.path.lexists(d/name):assert os.readlink(d/name)==target
            else:os.symlink(target,d/name)
        def put(i):
            payload=f'perf-{count}-{i:05d}\n'.encode()
            return req('PUT',f'/files/sr/perf-20260927/{count}/f{i:05d}',payload)
        started=time.monotonic()
        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool: list(pool.map(put,range(count)))
        end=time.monotonic()+180
        while True:
            entries=req('GET',f'/list?dir=/sr/perf-20260927/{count}&pending=1')['entries']
            if len(entries)==count+5 and all(x.get('state','committed')=='committed' for x in entries):break
            assert time.monotonic()<end
            time.sleep(.5)
        print(json.dumps(dict(prepared=count,seconds=time.monotonic()-started)),flush=True)
else:
    assert mode in ('before','after') and not os.path.ismount(root)
    counts=collections.Counter(); mutex=threading.Lock()
    class Proxy(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            with mutex:counts[self.path.split('?')[0].split('/')[1]]+=1
            c=http.client.HTTPConnection(host,7501,timeout=120)
            c.request('GET',self.path);r=c.getresponse();data=r.read();c.close()
            self.send_response(r.status);self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data)
        def log_message(self,*args):pass
    server=http.server.ThreadingHTTPServer(('127.0.0.1',7599),Proxy)
    threading.Thread(target=server.serve_forever,daemon=True).start()
    with open(base/f'fuse-{mode}.log','ab') as log:
        p=subprocess.Popen([str(base/'tape-fuse'),'-e','127.0.0.1:7599','--cache-dir',str(base/'cache'),str(root)],stdout=log,stderr=log)
        try:
            for _ in range(100):
                if os.path.ismount(root):break
                assert p.poll() is None;time.sleep(.1)
            assert os.path.ismount(root)
            def resources():
                stat=Path(f'/proc/{p.pid}/stat').read_text().split()
                status=Path(f'/proc/{p.pid}/status').read_text().splitlines()
                return dict(cpu_ticks=int(stat[13])+int(stat[14]),rss_kib=int(next(x for x in status if x.startswith('VmRSS:')).split()[1]))
            results=[]
            for n in (100,1000):
                for repeat in range(5):
                    with mutex:counts.clear()
                    before=resources();start=time.perf_counter()
                    with os.scandir(root/f'sr/perf-20260927/{n}') as it: entries=[(x.name,x.is_symlink(),x.is_dir(follow_symlinks=False)) for x in it]
                    elapsed=time.perf_counter()-start;after=resources()
                    assert len(entries)==n+5 and sum(e[1] for e in entries)==3 and sum(e[2] for e in entries)==2
                    assert {x[0] for x in entries}=={f'f{i:05d}' for i in range(n)}|{'empty','subdir','link','dangling','loop'}
                    with mutex:c=dict(counts)
                    result=dict(mode=mode,files=n,repeat=repeat,seconds=elapsed,requests=c,cpu_ticks=after['cpu_ticks']-before['cpu_ticks'],rss_kib=after['rss_kib'])
                    results.append(result);print(json.dumps(result),flush=True)
            (base/f'{mode}.json').write_text(json.dumps(results,indent=2)+'\n')
        finally:
            if os.path.ismount(root):subprocess.run(['fusermount','-u',str(root)],check=True)
            p.wait(timeout=30);server.shutdown()
