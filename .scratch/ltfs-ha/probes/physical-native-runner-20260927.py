"""LE 校验全部通过后，保留结果、重建相同空卷基线，再运行 Rust；任何失败立即停止。"""
import json,os,shutil,subprocess,time,xml.etree.ElementTree as ET
from pathlib import Path
assert os.environ.get('TAPE_RS_PERF_COMPARE')=='RC0018L9-RC0017L9'
b=Path('/root/tape-rs-io-20260927');lead='/opt/ibm/ltfsle/bin/leadm'
barcodes=('RC0018L9','RC0017L9');serial='11EB4A80F1'
def command(*args):return subprocess.check_output([lead,*args,'-s','localhost:17600'],text=True)
start=time.monotonic()
while not (b/'physical-le.complete').exists():
 if time.monotonic()-start>6*3600:raise RuntimeError('LE 未完成；未启动格式化/Rust测试')
 time.sleep(5)
rows=[json.loads(s) for s in (b/'le-measurements.jsonl').read_text().splitlines()]
assert rows[-1].get('kind')=='complete' and rows[-1]['passed']
assert all(r.get('verified') for r in rows if r.get('kind')=='read')
node=json.loads(command('node','show'));assert node['mount_point']==str(b/'le-mount')
for barcode in barcodes:
 show=json.loads(command('tape','show',barcode));assert show['slot_type']=='SLOT'
archive=b/'le-first-evidence';archive.mkdir(exist_ok=False)
for name in ['le-measurements.jsonl','le-start.log',*(x+'.json' for x in barcodes)]:shutil.copy2(b/name,archive/name)
shutil.copytree(b/'le-work',archive/'le-work')
print('LE results and cached indexes archived; resetting only RC test tapes',flush=True)
for barcode in barcodes:
 print('FORMAT START',barcode,flush=True);start=time.monotonic()
 print(command('tape','format','--force','--drive-serial',serial,'--volume-name','tape-rs-benchmark',barcode),flush=True)
 label=b/('le-work/55L3A7802K19LL01/volume_cache/'+barcode+'.label.ltfs.0')
 root=ET.fromstring(label.read_bytes());assert root.findtext('blocksize')=='524288'
 manifest=b/(barcode+'.json');data=json.loads(manifest.read_text());data['volume_uuid']=root.findtext('volumeuuid');assert data['volume_uuid'];manifest.write_text(json.dumps(data))
 print(command('tape','move','-L','homeslot',barcode),flush=True)
 print('FORMAT END',barcode,time.monotonic()-start,flush=True)
# 正常卸载私有 LE，等待它释放设备；不抢占预留、不强杀。
pid=int((b/'le.pid').read_text());cmdline=Path(f'/proc/{pid}/cmdline').read_bytes()
assert str(b/'le-mount').encode() in cmdline
subprocess.run(['umount',str(b/'le-mount')],check=True)
while Path(f'/proc/{pid}').exists():
 status=Path(f'/proc/{pid}/stat').read_text().split()
 if status[2]=='Z':break
 time.sleep(1)
assert not os.path.ismount(b/'le-mount')
assert subprocess.run(['pgrep','-x','ltfs'],capture_output=True).returncode==1
cli=str(b/'tape-rs-candidate')
pr=subprocess.check_output([cli,'pr-status','--device','/dev/sg3'],text=True)
(b/'native-pr-before.txt').write_text(pr);assert '持有者:     (无)' in pr,pr
inv=subprocess.check_output([cli,'inventory','--device','/dev/sg4','--no-drive-scan'],text=True)
(b/'native-inventory-before.txt').write_text(inv)
assert '驱动器   1 [空]' in inv
assert '存储槽   7 [载带]: RC0018L9' in inv and '存储槽   8 [载带]: RC0017L9' in inv
parameter=Path('/sys/module/sg/parameters/allow_dio');old=parameter.read_text()
(b/'native-allow-dio-before.txt').write_text(old)
try:
 parameter.write_text('1');assert parameter.read_text().strip()=='1'
 subprocess.run(['python3',str(b/'physical-native-20260927.py')],check=True)
 (b/'physical-native.complete').write_text('Rust full SHA and swaps passed')
finally:
 parameter.write_text(old);print('allow_dio restored to',old.strip(),flush=True)
