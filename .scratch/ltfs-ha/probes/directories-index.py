# Holo 原始 XML 证据扫描；不替代 LTFS 恢复器的有效性与追加位置检查。
import pathlib,re,sys,xml.etree.ElementTree as ET
base=pathlib.Path(sys.argv[1])
for p in sorted(base.glob('partitions/partition-*/data_*.seg')):
    data=p.read_bytes()
    found=[]
    for m in re.finditer(rb'<(ltfsindex|ltfsincrementalindex)\b.*?</\1>',data,re.S):
        x=ET.fromstring(m.group())
        found.append((x,m.group()))
    if not found: continue
    x,raw=found[-1]
    print(p.parent.name,x.tag,x.attrib,'generation',x.findtext('generationnumber'),'creator',x.findtext('creator'))
    print(raw.decode())
