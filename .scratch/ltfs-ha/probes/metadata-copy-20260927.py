"""仅复制已卸载的指定隔离介质；调用方先停止TAPERS集群并确认LE未分配。"""
import hashlib,json,os,pwd,shutil,sys
from pathlib import Path
assert os.environ.get('TAPE_RS_METADATA_COPY')=='closed-SR2501-only'
mode=sys.argv[1];assert mode in ('to-le','return')
root=Path('/var/lib/holo/storage-pools/pool1/cartridges')
base=Path('/home/rocky/holo-metadata-20260927')
src=root/('tapers/sr2501l8' if mode=='to-le' else 'lisa4300/sr2501l08')
dst=root/('lisa4300/sr2501l08' if mode=='to-le' else 'tapers/sr2501l8')
def sums(p):return {str(f.relative_to(p)):hashlib.sha256(f.read_bytes()).hexdigest() for f in p.rglob('*') if f.is_file()}
backup=base/(mode+'-destination');shutil.copytree(dst,backup);assert sums(dst)==sums(backup)
source=base/(mode+'-source');shutil.copytree(src,source);assert sums(src)==sums(source)
tmp=dst.with_name('.'+dst.name+'-metadata-candidate');old=dst.with_name('.'+dst.name+'-metadata-old')
assert not tmp.exists() and not old.exists()
shutil.copytree(src,tmp)
owner=pwd.getpwnam('holo')
for p in [tmp,*tmp.rglob('*')]:os.chown(p,owner.pw_uid,owner.pw_gid)
assert sums(src)==sums(tmp)
os.rename(dst,old)
try:os.rename(tmp,dst)
except BaseException:os.rename(old,dst);raise
assert sums(dst)==sums(src)
shutil.rmtree(old)
print(json.dumps(dict(mode=mode,sha256=sums(dst),passed=True)))
