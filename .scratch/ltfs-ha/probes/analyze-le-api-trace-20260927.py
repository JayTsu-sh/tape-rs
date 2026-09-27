"""把 LE 分段事件与 strace SG_IO 对齐；保留原始行号、CDB、sense及后台间隙。"""
import bisect
import collections
import json
import re
import sys
from pathlib import Path

base = Path(sys.argv[1])
events = [json.loads(x) for x in (base / 'le-api-events.jsonl').read_text().splitlines()]
phases = []
starts = {}
for e in events:
    if e['event'] == 'begin':
        starts[e['phase']] = e
    elif e['event'] in ('end', 'error'):
        begin = starts.pop(e['phase'])
        phases.append(dict(name=e['phase'], start=begin['time_ns']/1e9,
                           end=e['time_ns']/1e9, seconds=(e['time_ns']-begin['time_ns'])/1e9,
                           outcome=e['event'], commands=[]))
for name, e in starts.items():
    phases.append(dict(name=name, start=e['time_ns']/1e9, end=float('inf'),
                       seconds=None, outcome='pending', commands=[]))
phases.sort(key=lambda p:p['start'])
times = [p['start'] for p in phases]
ops = {0:'TUR',1:'REWIND',4:'FORMAT',5:'READ_BLOCK_LIMITS',8:'READ6',10:'WRITE6',
       16:'WRITE_FILEMARKS',17:'SPACE6',18:'INQUIRY',26:'MODE_SENSE6',27:'LOAD_UNLOAD',30:'PREVENT_ALLOW',
       52:'READ_POSITION',77:'LOG_SENSE',85:'MODE_SELECT10',90:'MODE_SENSE10',
       94:'PR_IN',95:'PR_OUT',130:'ALLOW_OVERWRITE',140:'READ_ATTRIBUTE',141:'WRITE_ATTRIBUTE',145:'SPACE16',
       146:'LOCATE16',165:'MOVE_MEDIUM',184:'READ_ELEMENT_STATUS'}
pending = {}
commands = []
unparsed = []


def decode_hex(value):
    return bytes.fromhex(value.replace('\\x',''))


def field(body, name, default=None):
    m = re.search(r'(?<!\w)'+name+r'=([^,}\s]+)', body)
    if not m:return default
    try:return int(m.group(1),0)
    except ValueError:return m.group(1)


for n, line in enumerate((base/'le-api-scsi.strace').read_text().splitlines(),1):
    m = re.match(r'^(\d+)\s+(\d+\.\d+)\s+(.*)$',line)
    if not m:continue
    tid, t, body = int(m[1]), float(m[2]), m[3]
    if '<unfinished ...>' in body:
        pending[tid]=(t,body.replace('<unfinished ...>',''),n)
        continue
    if body.startswith('<... ioctl resumed>'):
        if tid not in pending:
            unparsed.append(dict(line=n, reason='resumed ioctl without start'))
            continue
        t, first, n0 = pending.pop(tid)
        body=first+body[len('<... ioctl resumed>'):]; n=n0
    if 'SG_IO,' not in body:continue
    c = re.search(r'cmdp="([^"]+)"',body)
    elapsed = re.search(r'= (-?\d+)(?: [^<]+)? <([\d.]+)>$',body)
    if not c or not elapsed:
        unparsed.append(dict(line=n, reason='SG_IO without complete CDB/result'))
        continue
    cdb=decode_hex(c[1]); op=cdb[0]
    assert len(cdb)==field(body,'cmd_len'),(n,cdb.hex())
    dev=re.search(r'ioctl\(\d+<(.*?)<char',body)
    device=decode_hex(dev[1]).decode() if dev else '?'
    row=dict(line=n,tid=tid,start=t,end=t+float(elapsed[2]),device=device,
             opcode=f'{op:02x}',name=ops.get(op,f'OP_{op:02x}'),cdb=cdb.hex(),
             syscall_seconds=float(elapsed[2]),return_code=int(elapsed[1]),
             duration_ms=field(body,'duration'),status=field(body,'status'),
             host_status=field(body,'host_status'),driver_status=field(body,'driver_status'),
             resid=field(body,'resid'),dxfer_len=field(body,'dxfer_len'),
             flags=field(body,'flags'),info=field(body,'info'))
    sense=re.search(r'sbp="([^"]*)"',body)
    if sense and sense[1]:
        sense=decode_hex(sense[1]);row['sense_hex']=sense.hex()
        if len(sense)>=14 and sense[0]&0x7f in (0x70,0x71):
            row['sense_key']=sense[2]&15;row['asc']=sense[12];row['ascq']=sense[13]
    if op==0x10:row.update(count=int.from_bytes(cdb[2:5],'big'),immed=cdb[1]&1)
    if op==0x92:row.update(partition=cdb[3] if cdb[1]&2 else None,cp=bool(cdb[1]&2),dest_type=(cdb[1]>>3)&7,object=int.from_bytes(cdb[4:12],'big'))
    if op==0x1b:row.update(load=bool(cdb[4]&1),immed=cdb[1]&1)
    if op==0xa5:row.update(transport=int.from_bytes(cdb[2:4],'big'),source=int.from_bytes(cdb[4:6],'big'),destination=int.from_bytes(cdb[6:8],'big'))
    if op==0x8c:row.update(partition=cdb[7],attribute=f'{int.from_bytes(cdb[8:10],"big"):04x}')
    if op==0x8d:
        payload=re.search(r'dxferp="([^"]*)"',body)
        if payload:
            raw=decode_hex(payload[1])
            row.update(partition=cdb[7],attribute=f'{int.from_bytes(raw[4:6],"big"):04x}')
    if op in (8,10):
        info = row['info'] or 0
        if isinstance(info,str):
            bits = {'SG_INFO_CHECK':1,'SG_INFO_DIRECT_IO':2,'SG_INFO_MIXED_IO':4,'SG_INFO_OK':0}
            info = sum(bits[x] for x in info.split('|'))
        row['io_mode']={0:'indirect',2:'direct',4:'mixed'}.get(info&6,'unknown')
    i=bisect.bisect_right(times,t)-1
    if i>=0 and t<=phases[i]['end']:
        row['phase']=phases[i]['name'];phases[i]['commands'].append(row)
    else:row['phase']='between-phases'
    commands.append(row)
commands.sort(key=lambda x:x['start'])
for p in phases:
    p['opcode_counts']=dict(collections.Counter(c['name'] for c in p['commands']))
    p['scsi_seconds']=sum(c['syscall_seconds'] for c in p['commands'])
    if p['outcome']=='pending':p['end']=None
result=dict(phases=phases,commands=commands,pending_trace_calls=len(pending),unparsed=unparsed,
            between_phases=[c for c in commands if c['phase']=='between-phases'])
(base/'le-api-decoded.json').write_text(json.dumps(result,indent=2,ensure_ascii=False)+'\n')
for p in phases:
    print(p['name'],p['seconds'],p['opcode_counts'])
print('commands',len(commands),'between',len(result['between_phases']),'pending',len(pending))
print('unparsed',unparsed)
if (base/'le-api-trace.complete').exists():
    assert not pending and not unparsed, '已完成 trace 存在未解析的 ioctl'
    assert all(p['outcome']=='end' for p in phases), '已完成 trace 存在失败或未结束阶段'
