"""隔离 ltfsd 专用：延迟 ioctl 并在首个索引 WRITE(6) 返回后终止进程。"""
import json,os,pathlib,signal,subprocess,time
base=pathlib.Path('/home/rocky/tape-rs-xattr-ha-20260925')
assert os.environ.get('TAPE_RS_XATTR_CUT')=='RT2502L8'
pids=subprocess.check_output(['pgrep','-x','ltfsd'],text=True).split();assert len(pids)==1
pid=int(pids[0]);assert str(base/'ltfsd')==os.readlink(f'/proc/{pid}/exe')
s=json.loads((base/'test-data/status.json').read_text());assert s['role']=='Leader' and 'RT2502L8' in s['local']
assert s['pools'][0]['tapes']==['RT2502L8']
tids=[int(p.name) for p in pathlib.Path(f'/proc/{pid}/task').iterdir() if (p/'comm').read_text().strip()=='ltfsd-exec'];assert len(tids)==1
trace=base/'cut.trace';log=open(base/'cut-strace.log','w')
tracer=subprocess.Popen(['/usr/bin/strace','-p',str(tids[0]),'-e','trace=ioctl','-e','inject=ioctl:delay_enter=500ms','-v','-xx','-s','96','-tt','-T','-o',str(trace)],stdout=log,stderr=log)
try:
    deadline=time.monotonic()+10
    while f'TracerPid:\t{tracer.pid}' not in pathlib.Path(f'/proc/{pid}/task/{tids[0]}/status').read_text():
        assert time.monotonic()<deadline
        time.sleep(.05)
    (base/'cut.ready').write_text(json.dumps({'pid':pid,'tid':tids[0],'round':s['executor']['round']}))
    deadline=time.monotonic()+180
    while time.monotonic()<deadline:
        lines=trace.read_text().splitlines() if trace.exists() else []
        writes=[line for line in lines if 'cmdp="\\x0a' in line and ') = 0' in line and 'status=0,' in line]
        if writes:
            # The next ioctl is held before entry; no closing filemark may have returned.
            last=lines.index(writes[0]);assert not any('cmdp="\\x10' in l and ') = 0' in l for l in lines[last+1:])
            os.kill(pid,signal.SIGKILL)
            (base/'cut.event').write_text(json.dumps({'pid':pid,'round':s['executor']['round'],'write':writes[0],'lines_at_kill':lines},indent=2))
            print('PASS killed after index WRITE(6), before closing filemark',flush=True)
            break
        time.sleep(.02)
    else:raise TimeoutError('no index write observed; no kill performed')
finally:
    if tracer.poll() is None:
        tracer.terminate()
    tracer.wait(timeout=10)
