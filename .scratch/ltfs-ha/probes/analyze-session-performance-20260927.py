"""解码候选debug CDB计时，不把秒级日志时间戳冒充精确调用边界。"""
import collections
import json
import re
import statistics
import sys
from pathlib import Path

base = Path(sys.argv[1])
result = {}
for prefix in ('session', 'session2'):
    events_path = base / f'{prefix}-events.jsonl'
    if not events_path.exists():
        continue
    events = [json.loads(line) for line in events_path.read_text().splitlines()]
    commands = []
    mounts = 0
    for node in range(1, 4):
        log = base / f'{prefix}-node{node}.log'
        if not log.exists():
            continue
        source = log.read_text()
        mounts += source.count('LTFS label:')
        for line_num, line in enumerate(source.splitlines(), 1):
            if 'SG_IO:' not in line:
                continue
            match = re.search(r'SG_IO: device=(\S+) cdb=\[([^]]+)\] bytes=(\d+) wall_us=(\d+) duration_ms=(\d+) result=(.*?) status=(\S+) host=(\S+) driver=(\S+) resid=(-?\d+) info=(\S+) sense=\[([^]]*)\]', line)
            assert match, (log.name, line_num)
            cdb = bytes.fromhex(match[2].replace(',', ' '))
            row = dict(node=node, line=line_num, device=match[1], cdb=cdb.hex(), opcode=cdb[0], bytes=int(match[3]),
                       seconds=int(match[4])/1e6, duration_ms=int(match[5]), ioctl_result=match[6],
                       status=int(match[7], 0), host=int(match[8], 0), driver=int(match[9], 0),
                       resid=int(match[10]), info=int(match[11], 0), sense=match[12])
            if cdb[0] == 0x10:
                row.update(fm_count=int.from_bytes(cdb[2:5], 'big'), immed=bool(cdb[1]&1))
            if cdb[0] == 0x92:
                row.update(cp=bool(cdb[1]&2), partition=cdb[3], dest_type=(cdb[1]>>3)&7, block=int.from_bytes(cdb[4:12], 'big'))
            commands.append(row)
    fm0 = [c['seconds'] for c in commands if c.get('fm_count') == 0]
    puts = [e for e in events if e['phase'] == 'put']
    modes = collections.Counter(c['info'] & 6 for c in commands if c['opcode'] in (8, 10) and c['ioctl_result'] == 'Ok(0)')
    result[prefix] = dict(mount_count=mounts, puts=puts,
                          put_seconds_total=sum(e['seconds'] for e in puts),
                          fm0_seconds=fm0, fm0_median=statistics.median(fm0) if fm0 else None,
                          opcode_counts=dict(collections.Counter(f'{c["opcode"]:02x}' for c in commands)),
                          read_write_modes=dict(modes), commands=commands, events=events)
    print(prefix, 'mounts', mounts, 'puts', len(puts), 'sum', result[prefix]['put_seconds_total'], 'FM0 median', result[prefix]['fm0_median'])
(base / 'session-decoded.json').write_text(json.dumps(result, ensure_ascii=False, indent=2)+'\n')
