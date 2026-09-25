# 只读透明代理；记录旧执行者的实际响应，不伪造故障。
import argparse, socket, threading, json
p=argparse.ArgumentParser();p.add_argument('--upstream',required=True);a=p.parse_args()
assert a.upstream in ['10.131.9.71','10.131.9.72','10.131.9.74']
def handle(c):
    with c:
        c.settimeout(60);f=c.makefile('rb');lines=[]
        while True:
            line=f.readline(65536)
            if not line:return
            lines.append(line)
            if line==b'\r\n':break
        assert lines[0].startswith(b'GET ')
        with socket.create_connection((a.upstream,7501),timeout=60) as s:
            s.sendall(b''.join(lines));r=s.makefile('rb');headers=[];length=0
            while True:
                line=r.readline(65536)
                if not line:raise EOFError('response header')
                headers.append(line)
                if line.lower().startswith(b'content-length:'):length=int(line.split(b':',1)[1])
                if line==b'\r\n':break
            body=r.read(length);assert len(body)==length
            status=int(headers[0].split()[1]);print(json.dumps({'request':lines[0].decode().strip(),'status':status,'length':length,'error':body.decode() if status>=400 else None}),flush=True)
            c.sendall(b''.join(headers)+body)
with socket.socket() as l:
    l.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1);l.bind(('127.0.0.1',7592));l.listen()
    print('READY',flush=True)
    while True:
        c,_=l.accept();threading.Thread(target=handle,args=(c,),daemon=True).start()
