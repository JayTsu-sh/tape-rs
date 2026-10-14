"""只读提取Holo DTV2未压缩数据中的末次Full；辅助证据，不替代实际挂载。"""
import hashlib, json, os, re, struct
from pathlib import Path
import xml.etree.ElementTree as ET
assert os.environ.get('TAPE_RS_SYNC_BOUNDARY')=='SR2501L8-isolated'
src=Path('/var/lib/holo/storage-pools/pool1/cartridges/tapers/sr2501l8')
result={}
for n in (0,1):
    blocks=[]
    for p in sorted((src/'partitions'/f'partition-{n}').glob('data_*.seg')):
        data=p.read_bytes();assert data[96:100]==b'DTV2'
        offset=100
        while offset<len(data):
            assert offset+24<=len(data)
            blob,codec,logical,stored,checksum=struct.unpack_from('<QB3xIII',data,offset)
            assert codec==0 and logical==stored
            offset+=24;assert offset+stored<=len(data)
            blocks.append(data[offset:offset+stored]);offset+=stored
    xml=re.findall(rb'<ltfsindex\b.*?</ltfsindex>',b''.join(blocks),re.S)[-1]
    root=ET.fromstring(xml)
    assert b'sync-20260927' in xml and b'<key>ltfs.sync</key>' not in xml
    result[str(n)]=dict(generation=int(root.findtext('generationnumber')),files=len(root.findall('.//file')),sha256=hashlib.sha256(xml).hexdigest())
assert result['0']['generation']==result['1']['generation']
print(json.dumps(result))
