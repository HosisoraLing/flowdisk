#!/usr/bin/env python3
"""Standard-library integration checks with an adversarial OpenList/CDN fixture.
Run: cargo build && python3 tests/integration.py target/debug/flowdisk
"""
import hashlib
import http.cookiejar
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

TOKEN = 'integration-test-access-token-000000000'
PAYLOAD = bytes(range(256)) * (80 * 1024 + 7)  # > 20 MiB, includes a short last segment
CHANGED = PAYLOAD.translate(bytes.maketrans(bytes(range(256)), bytes(reversed(range(256)))))
COUNTS = {}
REQUESTS = []
ACTIVE = 0
PEAK = 0
REVISION = 1
LOCK = threading.Lock()
SIZES = {'ranged': len(PAYLOAD), 'single': len(PAYLOAD), 'expired': len(PAYLOAD), 'truncated': len(PAYLOAD), 'throttle': len(PAYLOAD), 'resume': len(PAYLOAD), 'changed': len(PAYLOAD), 'bad': len(PAYLOAD), 'empty': 0, 'capped':len(PAYLOAD), 'flaky':len(PAYLOAD), 'thunder':len(PAYLOAD), 'quarkproxy':len(PAYLOAD)}

class Server(ThreadingHTTPServer):
    daemon_threads = True
    def handle_error(self, request, client_address):
        pass  # cancellation deliberately breaks fixture streams

class Fixture(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    def log_message(self, *_): pass
    def send_json(self, data):
        raw = json.dumps(data).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', len(raw))
        self.end_headers()
        self.wfile.write(raw)
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get('Content-Length', 0))) or '{}')
        if self.path == '/api/auth/login':
            self.send_json({'code': 200, 'data': {'token': 'gateway-secret-never-expose'}})
            return
        if self.headers.get('Authorization') != 'gateway-secret-never-expose':
            self.send_json({'code': 401, 'data': None})
            return
        kind = body.get('path', '/').strip('/')
        if self.path == '/api/fs/list':
            self.send_json({'code':200,'data':{'content':[{'name':k,'size':v,'is_dir':False,'modified':'2026-10-15T00:00:00Z'} for k,v in SIZES.items()], 'total':len(SIZES)}})
        elif self.path == '/api/fs/get':
            route='p' if kind in ('thunder','quarkproxy') else 'cdn'
            self.send_json({'code':200,'data':{'name':kind+'.bin','size':SIZES.get(kind,0),'is_dir':False,'modified':f'2026-10-{15+REVISION if kind == "changed" else 15}T00:00:00Z','hash_info':{},'provider':'ThunderExpert' if kind=='thunder' else 'Quark','raw_url':f'http://127.0.0.1:{self.server.server_port}/{route}/{kind}'}})
        elif self.path == '/api/fs/link':
            with LOCK:
                COUNTS[kind] = COUNTS.get(kind,0) + 1
                version = COUNTS[kind]
            self.send_json({'code':200,'data':{'url':f'http://127.0.0.1:{self.server.server_port}/cdn/{kind}?v={version}', 'header':{'X-Provider-Auth':['private-cookie'],'User-Agent':['unit-thunder' if kind=='thunder' else 'pan.baidu.com']},'concurrency':0,'max_range':4*1024*1024 if kind in ('capped','flaky') else 0}})
        else:
            self.send_json({'code':500,'message':'fixture endpoint not found'})
    def do_GET(self):
        global ACTIVE, PEAK
        proxy=self.path.startswith('/p/')
        kind = self.path.split('/p/' if proxy else '/cdn/')[-1].split('?')[0]
        if proxy and kind=='thunder':
            with LOCK:COUNTS['thunder_proxy']=COUNTS.get('thunder_proxy',0)+1
            self.send_error(500);return
        if kind not in SIZES:
            self.send_error(404); return
        if not (proxy and kind=='quarkproxy') and (self.headers.get('X-Provider-Auth') != 'private-cookie' or self.headers.get('User-Agent') != ('unit-thunder' if kind=='thunder' else 'pan.baidu.com')):
            self.send_error(403); return
        if kind == 'expired' and self.path.endswith('?v=1'):
            self.send_error(403); return
        requested = self.headers.get('Range')
        if kind == 'empty' and requested:
            self.send_response(416); self.send_header('Content-Range','bytes */0'); self.send_header('Content-Length','0'); self.end_headers(); return
        first, last = 0, SIZES[kind] - 1
        if requested and kind != 'single':
            first, last = map(int, requested.removeprefix('bytes=').split('-'))
        if kind in ('capped','flaky') and last-first+1>4*1024*1024:
            self.send_error(403);return
        ranged = requested and kind != 'single'
        with LOCK:
            REQUESTS.append((kind,first,last, bool(requested)))
            seen = len([r for r in REQUESTS if r[0]==kind and r[2]>0])
        if kind == 'throttle' and seen == 1 and last > 0:
            self.send_response(429); self.send_header('Retry-After','1'); self.send_header('Content-Length','0'); self.end_headers(); return
        self.send_response(206 if ranged else 200)
        if ranged:
            self.send_header('Content-Range',f'bytes {first}-{last + (1 if kind == "bad" and last else 0)}/{SIZES[kind]}')
        self.send_header('Content-Length',last-first+1)
        self.send_header('ETag',f'"fixture-{REVISION if kind == "changed" else 1}"')
        self.end_headers()
        if last == 0 and requested:
            self.wfile.write(PAYLOAD[:1]); return
        with LOCK:
            ACTIVE += 1; PEAK = max(PEAK,ACTIVE)
        try:
            payload = CHANGED if kind == 'changed' and REVISION == 2 else PAYLOAD
            end = min(last,first+524288-1) if kind=='flaky' else (first + last)//2 if kind == 'truncated' and seen == 1 else last
            for start in range(first,end+1,65536):
                self.wfile.write(payload[start:min(start+65536,end+1)])
                self.wfile.flush()
                time.sleep(.025 if kind in ('resume','changed') else .002)
            if end != last:
                self.close_connection = True
        except (BrokenPipeError, ConnectionResetError): pass
        finally:
            with LOCK: ACTIVE -= 1

def free_port():
    with socket.socket() as s:
        s.bind(('127.0.0.1',0)); return s.getsockname()[1]

class App:
    def __init__(self, binary, root, fixture):
        self.port = free_port()
        self.base = f'http://127.0.0.1:{self.port}'
        self.env = dict(os.environ,APP_TOKEN=TOKEN,OPENLIST_PASSWORD='test-secret',OPENLIST_URL=f'http://127.0.0.1:{fixture.server_port}',BIND=f'127.0.0.1:{self.port}',DATA_DIR=str(root/'data'),DOWNLOAD_DIR=str(root/'downloads'),CONNECTIONS_PER_FILE='4')
        self.binary = str(Path(binary).resolve())
        self.log = open(root/'server.log','a')
        self.start()
    def start(self):
        self.proc = subprocess.Popen([self.binary],env=self.env,stdout=self.log,stderr=self.log)
        for _ in range(100):
            if self.proc.poll() is not None: raise AssertionError('server failed to start')
            try:
                urllib.request.urlopen(self.base+'/healthz',timeout=1).close(); return
            except (OSError,urllib.error.URLError): time.sleep(.05)
        raise AssertionError('startup timed out')
    def stop(self):
        self.proc.terminate(); self.proc.wait(timeout=15)
        assert self.proc.returncode == 0, self.proc.returncode
    def api(self,path,body=None,method=None,auth=True):
        headers={'Content-Type':'application/json'}
        if auth: headers['Authorization']='Bearer '+TOKEN
        req=urllib.request.Request(self.base+'/api'+path,data=json.dumps(body).encode() if body is not None else None,headers=headers,method=method)
        with urllib.request.urlopen(req,timeout=35) as res: return json.load(res)
    def create(self,kind): return self.api('/jobs',{'path':'/'+kind})['id']
    def job(self,id): return next(j for j in self.api('/jobs') if j['id']==id)
    def wait(self,id,status='completed',timeout=90):
        end=time.monotonic()+timeout
        while time.monotonic()<end:
            job=self.job(id)
            if job['status']==status: return job
            if job['status']=='failed' and status!='failed': raise AssertionError(job)
            time.sleep(.1)
        raise AssertionError(('timeout',self.job(id)))
    def verify(self,id,payload=PAYLOAD):
        job=self.wait(id)
        assert job['sha256']==hashlib.sha256(payload).hexdigest(),job
        req=urllib.request.Request(self.base+f'/api/jobs/{id}/file',headers={'Authorization':'Bearer '+TOKEN})
        with urllib.request.urlopen(req) as res:
            assert hashlib.sha256(res.read()).hexdigest()==job['sha256']
            etag=res.headers['ETag'];assert res.headers['Accept-Ranges']=='bytes'
        for endpoint in [f'/jobs/{id}/file',f'/local/{id}/file']:
            def fetch(headers=None, method=None):
                return urllib.request.urlopen(urllib.request.Request(self.base+'/api'+endpoint,headers={'Authorization':'Bearer '+TOKEN,**(headers or {})},method=method))
            with fetch(method='HEAD') as res: assert res.status==200 and int(res.headers['Content-Length'])==len(payload) and not res.read()
            if payload:
                with fetch({'Range':'bytes=17-111','If-Range':etag}) as res:
                    assert res.status==206 and res.read()==payload[17:112] and res.headers['Content-Range']==f'bytes 17-111/{len(payload)}'
                with fetch({'Range':'bytes=-23'}) as res: assert res.status==206 and res.read()==payload[-23:]
                with fetch({'Range':'bytes=17-111','If-Range':'"changed"'}) as res: assert res.status==200 and res.read()==payload
                # Simulate an interrupted client: join the prefix with a later range response.
                with fetch({'Range':'bytes=0-1048575'}) as res: prefix=res.read()
                with fetch({'Range':'bytes=1048576-','If-Range':etag}) as res: assert prefix+res.read()==payload
            try: fetch({'Range':f'bytes={len(payload)}-'})
            except urllib.error.HTTPError as error: assert error.code==416 and error.headers['Content-Range']==f'bytes */{len(payload)}'
            else: raise AssertionError('invalid resume range accepted')
        assert job['active_ms']>0 and job['speed_history']
        if job['transferred_bytes']>0: assert job['peak_speed']>0 and job['average_speed']>0
        return job

def main():
    global REVISION
    binary=sys.argv[1] if len(sys.argv)>1 else 'target/debug/flowdisk'
    server=Server(('127.0.0.1',0),Fixture)
    threading.Thread(target=server.serve_forever,daemon=True).start()
    with tempfile.TemporaryDirectory(prefix='flowdisk-test-') as tmp:
        root=Path(tmp)
        app=App(binary,root,server)
        try:
            try: app.api('/jobs',auth=False); raise AssertionError('missing auth accepted')
            except urllib.error.HTTPError as e: assert e.code==401
            cookie=http.cookiejar.CookieJar()
            browser=urllib.request.build_opener(urllib.request.HTTPCookieProcessor(cookie))
            req=urllib.request.Request(app.base+'/api/session',data=json.dumps({'token':TOKEN}).encode(),headers={'Content-Type':'application/json'})
            with browser.open(req) as r: assert 'HttpOnly' in r.headers['Set-Cookie'] and 'SameSite=Strict' in r.headers['Set-Cookie']
            assert json.load(browser.open(app.base+'/api/jobs'))==[]
            print('PASS auth, HttpOnly cookie, browse',flush=True)
            assert len(app.api('/browse')['content'])==len(SIZES)
            for kind in ['ranged','single','expired','truncated','throttle','empty','capped','flaky','thunder','quarkproxy']:
                id=app.create(kind)
                checked=app.verify(id,b'' if kind=='empty' else PAYLOAD)
                if kind!='empty': assert checked['transferred_bytes']>=len(PAYLOAD)
                print(f'PASS {kind}: byte-exact download',flush=True)
            assert COUNTS.get('thunder_proxy',0)==0 and COUNTS.get('quarkproxy',0)==0
            assert all(last-first+1<=4*1024*1024 for kind,first,last,_ in REQUESTS if kind in ('capped','flaky'))
            assert PEAK>=2,('no real concurrency',PEAK)
            assert COUNTS['expired']>=2
            assert any(kind=='truncated' and first>0 and first%(8*1024*1024)!=0 for kind,first,last,r in REQUESTS)
            print(f'PASS parallel requests: observed {PEAK} simultaneous streams',flush=True)
            for kind in ['resume','changed']:
                id=app.create(kind)
                deadline=time.monotonic()+20
                parts=root/'data'/'parts'/id
                while time.monotonic()<deadline:
                    if parts.exists() and sum(p.stat().st_size for p in parts.iterdir() if p.name.isdigit())>2*1024*1024: break
                    time.sleep(.03)
                app.api(f'/jobs/{id}/pause',{},'POST'); app.wait(id,'paused')
                partial=sum(p.stat().st_size for p in parts.iterdir() if p.name.isdigit())
                assert 0<partial<len(PAYLOAD),partial
                app.stop(); app.start()
                assert app.job(id)['status']=='paused'
                if kind=='changed': REVISION=2
                prior=len(REQUESTS)
                app.api(f'/jobs/{id}/resume',{},'POST')
                app.verify(id,CHANGED if kind=='changed' else PAYLOAD)
                if kind=='resume': assert any(k==kind and first>0 and first%(8*1024*1024)!=0 for k,first,last,r in REQUESTS[prior:])
                else: assert any(k==kind and first==0 and last>0 for k,first,last,r in REQUESTS[prior:])
                print(f'PASS {kind}: pause, process restart, safe continuation',flush=True)
            id=app.create('resume')
            parts=root/'data'/'parts'/id
            deadline=time.monotonic()+20
            while time.monotonic()<deadline:
                if parts.exists() and sum(p.stat().st_size for p in parts.iterdir() if p.name.isdigit())>2*1024*1024: break
                time.sleep(.03)
            app.proc.kill(); app.proc.wait(timeout=10); app.start()
            assert app.job(id)['status']=='paused'
            app.api(f'/jobs/{id}/resume',{},'POST'); app.verify(id)
            print('PASS abrupt process kill: recovered partial download safely',flush=True)
            id=app.create('bad'); failed=app.wait(id,'failed')
            assert 'Content-Range' in failed['error']
            assert not (root/'downloads'/id/'bad.bin').exists()
            assert 'gateway-secret' not in json.dumps(app.api('/jobs'))
            print('PASS malformed ranges rejected, no corrupted output or leaked tokens',flush=True)
            app.api(f'/jobs/{id}',method='DELETE')
            assert not (root/'data'/'parts'/id).exists()
            print('ALL INTEGRATION CHECKS PASSED',flush=True)
        finally:
            if app.proc.poll() is None: app.stop()
            server.shutdown()

if __name__=='__main__': main()
