# 仅在指定验收挂载点读取原文件，不修改磁带数据。
import hashlib, json, mmap, os
root=os.environ['TAPE_RS_READ_UPGRADE_MOUNT']
assert root=='/home/rocky/tape-rs-read-prod-20260924/mnt'
assert os.path.ismount(root)
expected=json.load(open('/home/rocky/tape-rs-read-prod-20260924/expected.json'))
assert len(expected)==9
for item in expected:
    assert item['path'].startswith('/rc/') and '..' not in item['path'].split('/')
    path=root+item['path']
    assert os.stat(path).st_size==item['length']
    with open(path,'rb') as f:
        assert hashlib.sha256(f.read()).hexdigest()==item['sha256']
        with mmap.mmap(f.fileno(),0,flags=mmap.MAP_SHARED,prot=mmap.PROT_READ) as m:
            assert hashlib.sha256(m[:]).hexdigest()==item['sha256']
print('PASS existing FUSE/Client: original 9 files lengths/SHA256/readonly shared mmap',flush=True)
