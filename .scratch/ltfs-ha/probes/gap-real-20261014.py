#!/usr/bin/env python3
"""受控 TS4300 实机 gap 验收。每阶段只运行一次，无自动重放，无 FORMAT/ERASE。
目标 lisa-3 / 55L3A78011W9LL01；仅 HH8 117018606F / sg2 调度。
用户已确认所有库存磁带均为测试带；LE 和 Rust 串行占用设备。
"""
import errno, hashlib, json, os, pathlib, subprocess, sys, time
ROOT=pathlib.Path('/root/tape-rs-gap-20261014')
RESULTS=ROOT/'results'
BIN=ROOT/'bin-v3'
CHANGER='/dev/sg3'  # sg5 FC path was lost after large DP commit; verified same TS4300 serial.
DIRECTORY='gap-20261014-basic'
PAYLOAD=b'tape-rs logical library gap acceptance 20261014\n'
SHA=hashlib.sha256(PAYLOAD).hexdigest()
PERF='2009f414fa6abf46885f6701a5f454b3f5af29afc1ff47b99bd4f42719d0d8c3'
FUSE_AA0013=b'physical FUSE gap test\n'
FUSE_AA0000=b'FUSE volume-specific AA0000L8\n'
FUSE_AA0013_SHA=hashlib.sha256(FUSE_AA0013).hexdigest()
FUSE_AA0000_SHA=hashlib.sha256(FUSE_AA0000).hexdigest()
VOLUMES=['AA0013L8','AA0000L8','8A0043L8','L00284L8','8A0047L8','L00285L8','8A0042L8']

def event(kind,**kw):
    with (RESULTS/'events.jsonl').open('a') as f:
        f.write(json.dumps(dict(time=time.time(),event=kind,**kw),ensure_ascii=False)+'\n')
    print(kind,kw,flush=True)

def args(round):
    return ['--library-changer',CHANGER,'--library-changer-serial','55L3A78011W9LL01',
        '--library-drive','117018606F=/dev/sg2','--library-state',str(ROOT/'library.sqlite'),
        '--library-node','250','--library-round',str(round)]

def run(tag,cmd,expected=0,debug=True):
    event('start',tag=tag,cmd=[str(x) for x in cmd]); start=time.monotonic()
    env=os.environ.copy()
    if debug: env['RUST_LOG']='tape_rs=debug'
    p=subprocess.run([str(x) for x in cmd],capture_output=True,env=env)
    (RESULTS/(tag+'.stdout')).write_bytes(p.stdout)
    (RESULTS/(tag+'.stderr')).write_bytes(p.stderr)
    event('finish',tag=tag,rc=p.returncode,seconds=time.monotonic()-start)
    if p.returncode!=expected:
        raise RuntimeError(f'{tag}: expected {expected}, got {p.returncode}: {p.stderr[-2000:].decode(errors="replace")}')
    return p

def ctl(tag,round,*command,expected=0):
    return run(tag,[BIN/'tape-library',*args(round),*command],expected)

def check_host():
    assert subprocess.check_output(['hostname']).decode().strip()=='lisa-3'
    # 不对一个已有 holder 的设备主动抢占，所有 phase 开始时应已正常清理。
    holders=subprocess.run(['fuser','/dev/sg2','/dev/sg3','/dev/sg4','/dev/sg5'],capture_output=True)
    if holders.returncode==0: raise RuntimeError('设备已被其他进程占用: '+holders.stdout.decode())
    mounts=subprocess.check_output(['findmnt','-rn','-o','TARGET,FSTYPE']).decode()
    if any('fuse' in line.lower() and ('ltfs' in line.lower() or str(ROOT) in line) for line in mounts.splitlines()):
        raise RuntimeError('仍有 LTFS/FUSE 挂载')

def sha_file(path):
    h=hashlib.sha256();n=0
    with open(path,'rb') as f:
        while True:
            b=f.read(1024*1024)
            if not b:break
            h.update(b);n+=len(b)
    return h.hexdigest(),n

def basic():
    ctl('initial',202610140010,'status')
    for i,barcode in enumerate(VOLUMES): ctl('assign-'+barcode,202610140010+i,'assign','test',barcode)
    ctl('assigned',202610140020,'status')
    for i,barcode in enumerate(VOLUMES[:2]):
        ctl('before-'+barcode,202610140021+i,'list','/test/'+barcode)
        run('fixture-'+barcode,[BIN/'library_gap_fixture',*args(202610140023+i),
            '--volume','/test/'+barcode,'--directory',DIRECTORY,'--write-test-data'])
    run('native-basic',[BIN/'library_acceptance',*args(202610140025),
        '--expect',f'/test/AA0013L8/{DIRECTORY}/marker.bin={SHA}',
        '--expect',f'/test/AA0000L8/{DIRECTORY}/marker.bin={SHA}',
        '--expect',f'/test/AA0013L8/perf64m.bin={PERF}',
        '--report',RESULTS/'native-basic.json'])
    ctl('basic-final',202610140026,'status')

def resume_basic():
    assert (RESULTS/'basic.failed').exists()
    # 结果未定后先校验已落DP的marker与原文件，只补IP，不重复append marker。
    run('native-repair-ip',[BIN/'library_acceptance',*args(202610140071),
        '--expect',f'/test/AA0013L8/{DIRECTORY}/marker.bin={SHA}',
        '--expect',f'/test/AA0013L8/perf64m.bin={PERF}',
        '--checkpoint','--report',RESULTS/'native-repair-ip.json'])
    ctl('resume-before-AA0000L8',202610140072,'list','/test/AA0000L8')
    run('resume-fixture-AA0000L8',[BIN/'library_gap_fixture',*args(202610140073),
        '--volume','/test/AA0000L8','--directory',DIRECTORY,'--write-test-data'])
    run('native-basic',[BIN/'library_acceptance',*args(202610140074),
        '--expect',f'/test/AA0013L8/{DIRECTORY}/marker.bin={SHA}',
        '--expect',f'/test/AA0000L8/{DIRECTORY}/marker.bin={SHA}',
        '--expect',f'/test/AA0013L8/perf64m.bin={PERF}',
        '--report',RESULTS/'native-basic.json'])
    ctl('basic-final',202610140075,'status')
    (RESULTS/'basic.complete').write_text(str(time.time()))

def fuse(large=False):
    mount=ROOT/'mount'; mount.mkdir(exist_ok=True)
    round=202610140110 if large else 202610140080
    log=open(RESULTS/('fuse-large.log' if large else 'fuse-basic.log'),'wb')
    env=os.environ.copy();env['RUST_LOG']='tape_rs=debug'
    cmd=[str(BIN/'tape-fuse'),*args(round),'--cache-dir',str(ROOT/'cache'),str(mount)]
    p=subprocess.Popen(cmd,stdout=log,stderr=subprocess.STDOUT,env=env)
    (RESULTS/'fuse.pid').write_text(str(p.pid))
    try:
        deadline=time.monotonic()+60
        while not os.path.ismount(mount):
            if p.poll() is not None:raise RuntimeError('FUSE exited '+str(p.returncode))
            if time.monotonic()>deadline:raise RuntimeError('FUSE mount timeout')
            time.sleep(1)
        event('fuse-mounted',pid=p.pid,large=large)
        assert os.listdir(mount)==['test']
        assert sorted(os.listdir(mount/'test'))==sorted(VOLUMES)
        live=json.loads(ctl('fuse-live-status-'+('large' if large else 'basic'),round,'status').stdout)
        assert live['source']=='cached_inventory'
        assert live['library_serial']=='55L3A78011W9LL01'
        assert live.get('observed_at')
        if large:
            assert sha_file(mount/'test/AA0013L8/gap-20261014-large/marker.bin')[0]==SHA
            assert os.getxattr(mount/'test/AA0013L8','user.tapers.gap.20261014.0000')==b'x'*65536
            event('fuse-large-index-xattr-passed')
        for b in ['AA0013L8','AA0000L8','AA0013L8']:
            path=mount/'test'/b/DIRECTORY/'marker.bin'
            assert sha_file(path)[0]==SHA
            event('fuse-read',barcode=b,sha256=SHA)
        assert sha_file(mount/'test/AA0013L8/perf64m.bin')==(PERF,67108864)
        if not large:
            base=mount/'test/AA0013L8'/DIRECTORY
            path=base/'fuse-write.bin'
            with open(path,'xb') as f:
                f.write(FUSE_AA0013);f.flush();os.fsync(f.fileno())
            os.setxattr(path,'user.gap.binary',b'\x00\xff<>&')
            assert os.getxattr(path,'user.gap.binary')==b'\x00\xff<>&'
            os.rename(path,base/'fuse-renamed.bin')
            os.symlink('fuse-renamed.bin',base/'fuse-link')
            assert (base/'fuse-link').read_bytes()==FUSE_AA0013
            try:os.rename(base/'fuse-renamed.bin',mount/'test/AA0000L8'/DIRECTORY/'cross.bin')
            except OSError as e:assert e.errno==errno.EXDEV
            else:raise AssertionError('cross-volume rename succeeded')
            other=mount/'test/AA0000L8'/DIRECTORY/'fuse-volume.bin'
            with open(other,'xb') as f:
                f.write(FUSE_AA0000);f.flush();os.fsync(f.fileno())
            assert sha_file(other)==(FUSE_AA0000_SHA,len(FUSE_AA0000))
            event('fuse-write-metadata-rename-symlink-exdev-passed')
    finally:
        if os.path.ismount(mount): run('umount-'+('large' if large else 'basic'),['umount',mount],debug=False)
        p.wait(timeout=600);log.close()
        if p.returncode!=0:raise RuntimeError('FUSE cleanup exit '+str(p.returncode))
    ctl('fuse-final-'+('large' if large else 'basic'),round+1,'status')

def management():
    ctl('blacklist',202610140090,'blacklist','AA0000L8','真实验收隔离')
    s=json.loads(ctl('blacklisted-status',202610140090,'status').stdout)
    row=next(v for v in s['volumes'] if v['barcode']=='AA0000L8');assert row['state']=='BLACKLISTED'
    ctl('blacklisted-refusal',202610140090,'list','/test/AA0000L8',expected=1)
    ctl('clear-blacklist',202610140091,'clear-blacklist','AA0000L8')
    ctl('unassign',202610140092,'unassign','AA0000L8')
    s=json.loads(ctl('need-assign-status',202610140092,'status').stdout)
    assert next(v for v in s['volumes'] if v['barcode']=='AA0000L8')['state']=='NEED_ASSIGN'
    ctl('reassign',202610140093,'assign','test','AA0000L8')

def large():
    run('large-fixture',[BIN/'library_gap_fixture',*args(202610140100),
        '--volume','/test/AA0013L8','--directory','gap-20261014-large','--large-index','--write-test-data'])
    old='/tmp/tape-rs-real-validation-20261013'
    run('old-load',[old,'load','--device',CHANGER,'--slot','1','--drive','1'])
    run('old-drive-load',[old,'drive-load','--device','/dev/sg2'])
    p=run('old-64m-budget-failure',[old,'ltfs-list','--device','/dev/sg2'],expected=1)
    assert '67108864' in p.stderr.decode(errors='replace')
    ctl('new-large-list',202610140101,'list','/test/AA0013L8')
    run('native-large',[BIN/'library_acceptance',*args(202610140102),
        '--expect',f'/test/AA0013L8/gap-20261014-large/marker.bin={SHA}',
        '--expect',f'/test/AA0013L8/{DIRECTORY}/fuse-renamed.bin={FUSE_AA0013_SHA}',
        '--expect',f'/test/AA0013L8/perf64m.bin={PERF}',
        '--expect',f'/test/AA0000L8/{DIRECTORY}/fuse-volume.bin={FUSE_AA0000_SHA}',
        '--report',RESULTS/'native-large.json'])
    fuse(large=True)

def resume_large():
    assert (RESULTS/'large.failed').exists()
    assert (RESULTS/'large-fc-recovery.json').exists()
    old='/tmp/tape-rs-real-validation-20261013'
    run('old-load',[old,'load','--device',CHANGER,'--slot','1','--drive','1'])
    run('old-drive-load',[old,'drive-load','--device','/dev/sg2'])
    p=run('old-64m-budget-failure',[old,'ltfs-list','--device','/dev/sg2'],expected=1)
    assert '67108864' in p.stderr.decode(errors='replace')
    ctl('new-large-list',202610140101,'list','/test/AA0013L8')
    run('native-large',[BIN/'library_acceptance',*args(202610140102),
        '--expect',f'/test/AA0013L8/gap-20261014-large/marker.bin={SHA}',
        '--expect',f'/test/AA0013L8/{DIRECTORY}/fuse-renamed.bin={FUSE_AA0013_SHA}',
        '--expect',f'/test/AA0013L8/perf64m.bin={PERF}',
        '--expect',f'/test/AA0000L8/{DIRECTORY}/fuse-volume.bin={FUSE_AA0000_SHA}',
        '--report',RESULTS/'native-large.json'])
    fuse(large=True)

if __name__=='__main__':
    phase=sys.argv[1];check_host()
    with (RESULTS/(phase+'.started')).open('x') as f:f.write(str(time.time()))
    try:
        globals()[phase]()
        (RESULTS/(phase+'.complete')).write_text(str(time.time()));event('phase-complete',phase=phase)
    except BaseException as e:
        (RESULTS/(phase+'.failed')).write_text(repr(e));event('phase-failed',phase=phase,error=repr(e));raise
