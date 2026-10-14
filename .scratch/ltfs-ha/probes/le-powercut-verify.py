import os, mmap, hashlib, json
root='/ltfs/TS1001L8'
prefix='rc/powercut-37b5d411-43d5-4d48-b188-e2ad3276e31c'
for n in range(1,4):
    path=f'{root}/{prefix}/f{n}'
    with open(path,'rb') as f:
        data=f.read(); assert data==bytes([n])*65536
        with mmap.mmap(f.fileno(),0,flags=mmap.MAP_SHARED,prot=mmap.PROT_READ) as m:
            assert m[:]==data
    print(json.dumps({'file':f'f{n}','bytes':len(data),'sha256':hashlib.sha256(data).hexdigest(),'readonly_mmap':'PASS'}),flush=True)
p=f'{root}/{prefix}'
assert os.path.isdir(p+'/empty') and os.listdir(p+'/empty')==[]
assert os.path.islink(p+'/link') and os.readlink(p+'/link')=='f1'
assert open(p+'/link','rb').read()==bytes([1])*65536
assert os.getxattr(p+'/f1','user.powercut.binary')==b'\x00\xff'
assert hashlib.sha256(open(root+'/rc/_probe.bin','rb').read()).hexdigest()=='49abd65bbf7f7e40c7055093ed2e3fd75f2f602f2c5fcf955c213e3135eb03f7'
print('PASS empty_directory symlink binary_xattr original_probe',flush=True)
