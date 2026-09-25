"""隔离 RT2502L8 gen71：卷提交gen72后、目录发布前中断。"""
import json,os,pathlib,signal,subprocess,time,sqlite3,re
base=pathlib.Path('/home/rocky/tape-rs-publication-cut-20260925')
assert os.environ.get('TAPE_RS_XATTR_CUT')=='RT2502L8'
pids=subprocess.check_output(['pgrep','-x','ltfsd'],text=True).split();assert len(pids)==1
pid=int(pids[0]);assert str(base/'ltfsd')==os.readlink(f'/proc/{pid}/exe')
s=json.loads((base/'test-data/status.json').read_text());assert s['role']=='Leader' and 'RT2502L8' in s['local']
assert s['pools'][0]['tapes']==['RT2502L8']
tids=[int(p.name) for p in pathlib.Path(f'/proc/{pid}/task').iterdir() if (p/'comm').read_text().strip()=='ltfsd-exec'];assert len(tids)==1
log_start=len((base/"test.log").read_text())
trace=base/'cut.trace';log=open(base/'cut-strace.log','w')
tracer=subprocess.Popen([str(base/'strace'),'-p',str(tids[0]),'-e','trace=ioctl','-e','inject=ioctl:delay_enter=200ms','-v','-xx','-s','96','-tt','-T','-o',str(trace)],stdout=log,stderr=log)
try:
    deadline=time.monotonic()+10
    while f'TracerPid:\t{tracer.pid}' not in pathlib.Path(f'/proc/{pid}/task/{tids[0]}/status').read_text():
        assert time.monotonic()<deadline
        time.sleep(.05)
    (base/'cut.ready').write_text(json.dumps({'pid':pid,'tid':tids[0],'round':s['executor']['round']}))
    deadline=time.monotonic()+180
    while time.monotonic()<deadline:
        lines=trace.read_text().splitlines() if trace.exists() else []
        recent=(base/'test.log').read_text()[log_start:]
        committed=re.search(r'commit: gen (\d+) @', recent)
        if committed:
            assert '执行线程: 已提交' not in recent, 'publication already passed'
            generation=int(committed.group(1))
            assert generation == 72, generation
            db=sqlite3.connect('file:'+str(base/'test-data/directory.db')+'?mode=ro',uri=True)
            rows=db.execute('select * from files where path=?',('/rc/xattr-fault-20260925',)).fetchall()
            if not rows: rows=db.execute('select * from files where path=?',('rc/xattr-fault-20260925',)).fetchall()
            assert len(rows)==1
            columns=[x[1] for x in db.execute('pragma table_info(files)')]
            record=dict(zip(columns,rows[0]));assert record['generation']==71,record
            assert 'uncommitted' not in json.dumps(record)
            os.kill(pid,signal.SIGKILL)
            (base/'cut.event').write_text(json.dumps({'pid':pid,'round':s['executor']['round'],'committed_generation':generation,'catalog_before_kill':record,'log':recent,'trace_tail':lines[-6:]},indent=2))
            print('PASS killed after tape commit before catalog publication',flush=True)
            break
        time.sleep(.02)
    else:raise TimeoutError('no index write observed; no kill performed')
finally:
    if tracer.poll() is None:
        tracer.terminate()
    tracer.wait(timeout=10)
