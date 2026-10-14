"""显式指定本轮正式集群验收挂载点；仅写唯一的验收目录。"""
import errno
import hashlib
import json
import os
import sys

root = os.environ['TAPE_RS_UPGRADE_MOUNT']
assert root == '/home/rocky/tape-rs-directories-prod-20260924/mnt'
assert os.path.ismount(root)
p = root + '/rc/directory-upgrade-20260924'
mode = sys.argv[1]
if mode == 'prepare':
    assert not os.path.exists(p)
    os.mkdir(p)
    os.mkdir(p + '/empty')
    try:
        os.rmdir(p)
    except OSError as e:
        assert e.errno == errno.ENOTEMPTY, e
    else:
        raise AssertionError('非空目录被删除')
else:
    assert os.path.isdir(p + '/empty')
    assert os.listdir(p + '/empty') == []
    if mode == 'cleanup':
        os.rmdir(p + '/empty')
        os.rmdir(p)
        assert not os.path.exists(p)
    else:
        assert mode == 'verify'
with open(sys.argv[2]) as f:
    for item in json.load(f):
        with open(root + item['path'], 'rb') as data:
            content = data.read()
        assert len(content) == item['length'], item['path']
        assert hashlib.sha256(content).hexdigest() == item['sha256'], item['path']
print('PASS', mode, '目录操作与原9个文件SHA256校验')
