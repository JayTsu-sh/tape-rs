"""显式实机门控；等待格式化成功，然后同主机 direct I/O 基线；始终恢复内核参数。"""
import json,os,subprocess,time
from pathlib import Path
assert os.environ.get('TAPE_RS_PERF_COMPARE')=='RC0018L9-RC0017L9'
b=Path('/root/tape-rs-io-20260927');lead='/opt/ibm/ltfsle/bin/leadm'
start=time.monotonic()
while not (b/'format-baseline.complete').exists():
 if time.monotonic()-start>7200:raise RuntimeError('等待格式化完成超时；未发出测试命令')
 time.sleep(5)
assert (b/'source.complete').exists()
node=json.loads(subprocess.check_output([lead,'node','show','-s','localhost:17600'],text=True))
assert node['mount_point']==str(b/'le-mount')
for barcode in ('RC0018L9','RC0017L9'):
 m=json.loads((b/(barcode+'.json')).read_text());assert m['volume_uuid']
 show=json.loads(subprocess.check_output([lead,'tape','show',barcode,'-s','localhost:17600'],text=True))
 assert show['slot_type']=='SLOT',show
 if show['assignment']=='UNASSIGNED':subprocess.run([lead,'tape','assign',barcode,'-s','localhost:17600'],check=True)
parameter=Path('/sys/module/sg/parameters/allow_dio');old=parameter.read_text()
(b/'le-allow-dio-before.txt').write_text(old)
try:
 parameter.write_text('1');assert parameter.read_text().strip()=='1'
 subprocess.run(['python3',str(b/'physical-le-20260927.py')],check=True)
 (b/'physical-le.complete').write_text('LE full SHA and swaps passed')
finally:
 parameter.write_text(old)
 print('allow_dio restored to',old.strip(),flush=True)
