import os,json,hashlib
assert os.environ['TAPE_RS_RENAME_LE_BARCODE']=='TS1000L8'
root='/ltfs/TS1000L8';p=root+'/rc/rename-interop-20260924'
e=json.load(open('/home/rocky/tape-rs-rename-20260924/expected.json'))
f=p+'/imported.bin'
assert hashlib.sha256(open(f,'rb').read()).hexdigest()==e['sha256']
assert os.stat(f).st_mtime_ns==e['mtime']
assert os.getxattr(f,'user.roundtrip.binary').hex()==e['xattr']
assert os.getxattr(p+'/imported-empty','user.directory.binary')==b'\x00\xffdirectory'
assert os.stat(p+'/imported-empty').st_mtime_ns==1790200000123456789
assert os.path.isdir(p+'/moved/empty')
assert not os.path.exists(p+'/source')
assert open(p+'/target','rb').read()==b'final writer'
assert open(p+'/lost-target/file','rb').read()==b'atomic lost response'
assert not os.path.exists(p+'/lost-source')
os.rename(f,root+'/rc/edit.bin')
os.rename(p+'/imported-empty',root+'/rc/dir-interop-20260924/le-empty')
os.rename(p+'/moved',p+'/source')
os.setxattr(root,'user.ltfs.sync',b'1')
print('PASS LE reads native rename and renames back to former names',flush=True)
