"""单次 HTTP 故障探针：只在指定方法/路径触发，等待外部停止 Leader 后释放。"""
import argparse,json,socket,threading,time
from pathlib import Path
p=argparse.ArgumentParser()
p.add_argument('--upstream',required=True);p.add_argument('--port',type=int,required=True)
p.add_argument('--method',required=True);p.add_argument('--path',required=True)
p.add_argument('--mode',choices=['partial','response'],required=True);p.add_argument('--event',required=True)
a=p.parse_args(); fired=False; lock=threading.Lock()
def header(f):
 lines=[]
 while True:
  line=f.readline(65537)
  if not line: raise EOFError('header EOF')
  lines.append(line)
  if line==b'\r\n':break
  if sum(map(len,lines))>65536:raise ValueError('header too large')
 attrs={}
 for line in lines[1:]:
  if b':' in line:
   k,v=line.split(b':',1);attrs[k.strip().lower()]=v.strip()
 return b''.join(lines),lines[0],attrs
def gate(detail):
 Path(a.event).write_text(json.dumps(detail))
 print('EVENT',json.dumps(detail),flush=True)
 deadline=time.monotonic()+90
 while not Path(a.event+'.release').exists():
  if time.monotonic()>deadline:raise TimeoutError('fault release not received')
  time.sleep(.05)
def handle(client):
 global fired
 try:
  with client, socket.create_connection((a.upstream,7501),timeout=120) as server:
   client.settimeout(120);cf=client.makefile('rb');sf=server.makefile('rb')
   raw,first,attrs=header(cf); method,path,_=first.decode().strip().split(' ')
   with lock:
    target=not fired and method==a.method and path.split('?')[0]==a.path
    if target:fired=True
   print('REQUEST',method,path,'target',target,flush=True)
   server.sendall(raw)
   if attrs.get(b'expect',b'').lower()==b'100-continue':
    rh,status,ra=header(sf)
    if status.split()[1]!=b'100':
     client.sendall(rh+sf.read(int(ra.get(b'content-length',b'0'))));return
    client.sendall(rh)
   left=int(attrs.get(b'content-length',b'0'));sent=0
   while left:
    block=cf.read(min(left,65536))
    if not block:raise EOFError('request body EOF')
    server.sendall(block);left-=len(block);sent+=len(block)
    if target and a.mode=='partial':
     gate(dict(method=method,path=path,mode=a.mode,forwarded=sent,remaining=left,upstream=a.upstream))
     return
   rh,status,ra=header(sf);body=sf.read(int(ra.get(b'content-length',b'0')))
   if target and a.mode=='response':
    code=int(status.split()[1]);assert code in (200,201),(code,body)
    gate(dict(method=method,path=path,mode=a.mode,status=code,body=body.decode(),upstream=a.upstream))
    return
   client.sendall(rh+body)
 except Exception as e:print('PROXY_ERROR',repr(e),flush=True)
with socket.socket() as listener:
 listener.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1);listener.bind(('127.0.0.1',a.port));listener.listen()
 print('READY',a.port,a.upstream,flush=True)
 while True:
  c,_=listener.accept();threading.Thread(target=handle,args=(c,),daemon=True).start()
