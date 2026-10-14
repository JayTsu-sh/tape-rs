#!/usr/bin/env python3
"""Rust 完成并释放设备后，LE 只读访问同一 65MiB+ 索引夹具；正常卸载。"""
import hashlib,json,os,pathlib,subprocess,time
ROOT=pathlib.Path('/root/tape-rs-gap-20261014'); RESULTS=ROOT/'results'
assert subprocess.check_output(['hostname']).decode().strip()=='lisa-3'
assert (RESULTS/'resume_large.complete').exists(), 'Rust 大索引验收未完成'
assert subprocess.run(['fuser','/dev/sg2','/dev/sg3','/dev/sg4','/dev/sg5'],capture_output=True).returncode!=0,'device holder present'
with (RESULTS/'le-large.started').open('x') as f:f.write(str(time.time()))
mount=ROOT/'le-mount';work=ROOT/'le-work';mount.mkdir(exist_ok=True);work.mkdir(exist_ok=True)
assert not os.path.ismount(mount)
log=open(RESULTS/'le-large.log','wb')
cmd=['/opt/ibm/ltfsle/bin/ltfs','-f','-o','changer_devname=/dev/sg3','-o','work_directory='+str(work),'-o','sync_type=unmount','-o','admin_port=17680',str(mount)]
p=subprocess.Popen(cmd,stdout=log,stderr=subprocess.STDOUT)
(RESULTS/'le-large.pid').write_text(str(p.pid))
result={'command':cmd,'pid':p.pid,'start':time.time(),'reads':[]}
try:
    deadline=time.monotonic()+600
    while not os.path.ismount(mount):
        if p.poll() is not None:raise RuntimeError('LE exited '+str(p.returncode))
        if time.monotonic()>deadline:raise RuntimeError('LE mount timeout')
        time.sleep(1)
    result['volumes']=sorted(os.listdir(mount))
    for path,sha in [('AA0013L8/gap-20261014-large/marker.bin',hashlib.sha256(b'tape-rs logical library gap acceptance 20261014\n').hexdigest()),('AA0013L8/perf64m.bin','2009f414fa6abf46885f6701a5f454b3f5af29afc1ff47b99bd4f42719d0d8c3')]:
        start=time.monotonic();h=hashlib.sha256();size=0
        with open(mount/path,'rb') as f:
            while True:
                data=f.read(1024*1024)
                if not data:break
                h.update(data);size+=len(data)
        actual=h.hexdigest();result['reads'].append(dict(path=path,sha256=actual,expected=sha,bytes=size,seconds=time.monotonic()-start));assert actual==sha
    attr=os.getxattr(mount/'AA0013L8','user.tapers.gap.20261014.0000')
    assert attr==b'x'*65536;result['large_root_xattr_verified']=True
except BaseException as e:
    result['error']=repr(e);raise
finally:
    if os.path.ismount(mount):
        u=subprocess.run(['umount',mount],capture_output=True)
        result['unmount_rc']=u.returncode;result['unmount_stderr']=u.stderr.decode(errors='replace')
        if u.returncode!=0:raise RuntimeError('LE normal unmount failed')
    try: result['exit']=p.wait(timeout=600)
    except subprocess.TimeoutExpired:
        p.terminate();result['exit_after_term']=p.wait(timeout=120)
    log.close();result['end']=time.time()
    (RESULTS/'le-large.json').write_text(json.dumps(result,ensure_ascii=False,indent=2))
if 'error' not in result and result['unmount_rc']==0 and result.get('exit')==0:
    (RESULTS/'le-large.complete').write_text(str(time.time()))
else:
    raise RuntimeError('LE 未正常退出')
