import os,hashlib,json
root='/ltfs/TS1001L7'
print('BEFORE indexVersion='+os.getxattr(root,'user.ltfs.indexVersion').decode(),flush=True)
data=bytes(range(256))*256+b'written by actual IBM LE 2.4.8.3\n'
p=root+'/rc/le-downgrade-20260924.bin'
with open(p,'xb') as f:
    f.write(data); f.flush(); os.fsync(f.fileno())
os.setxattr(p,'user.interop.binary',b'\x00\xffLE24')
os.setxattr(root,'user.ltfs.sync',b'1')
print(json.dumps({'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest(),'indexVersion':os.getxattr(root,'user.ltfs.indexVersion').decode(),'pool_uuid':os.getxattr(root,'user.ltfs.mediaPool.uuid').decode(),'mtime_ns':os.stat(p).st_mtime_ns}),flush=True)
assert os.getxattr(root,'user.ltfs.indexVersion')==b'2.4.0'
