#!/usr/bin/env python3
"""Small real BT lifecycle test. ARIA2_BIN must provide aria2c; no public trackers/accounts."""
import base64
import hashlib
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
from http.server import BaseHTTPRequestHandler
from urllib.parse import quote
from control import App, Gateway, Server
from integration import free_port

PAYLOAD=bytes(range(256))*1024

def bencode(value):
    if isinstance(value,int):return b'i'+str(value).encode()+b'e'
    if isinstance(value,bytes):return str(len(value)).encode()+b':'+value
    if isinstance(value,list):return b'l'+b''.join(map(bencode,value))+b'e'
    return b'd'+b''.join(bencode(k)+bencode(value[k])for k in sorted(value))+b'e'

def wait(app,ident,ready,timeout=45):
    deadline=time.monotonic()+timeout
    while time.monotonic()<deadline:
        row=next(x for x in app.api('/seeding')if x['id']==ident)
        if ready(row):return row
        assert not row['error'],row
        time.sleep(.15)
    raise AssertionError(('timeout',row))

def read_exact(sock,length):
    data=b''
    while len(data)<length:
        block=sock.recv(length-len(data))
        if not block:raise AssertionError('peer disconnected')
        data+=block
    return data

def leech(port,infohash):
    with socket.create_connection(('127.0.0.1',port),timeout=20)as peer:
        peer.settimeout(25)
        peer.sendall(b'\x13BitTorrent protocol'+b'\0'*8+infohash+b'-FDTEST-000000000001')
        answer=read_exact(peer,68);assert answer[28:48]==infohash
        peer.sendall(struct.pack('!IB',1,2))  # interested
        while True:
            size=struct.unpack('!I',read_exact(peer,4))[0]
            if size==0:continue
            message=read_exact(peer,size)
            if message[0]==1:break  # unchoked
        peer.sendall(struct.pack('!IBIII',13,6,0,0,16384))
        while True:
            size=struct.unpack('!I',read_exact(peer,4))[0]
            if not size:continue
            message=read_exact(peer,size)
            if message[0]==7:
                assert struct.unpack('!II',message[1:9])==(0,0)
                assert message[9:]==PAYLOAD[:16384]
                return

def run(binary):
    aria=os.environ.get('ARIA2_BIN','aria2c')
    source_port=free_port()
    class Tracker(BaseHTTPRequestHandler):
        def log_message(self,*_):pass
        def do_GET(self):
            body=bencode({b'interval':1,b'complete':1,b'incomplete':1,b'peers':socket.inet_aton('127.0.0.1')+struct.pack('!H',source_port)})
            self.send_response(200);self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
    tracker=Server(('127.0.0.1',0),Tracker);threading.Thread(target=tracker.serve_forever,daemon=True).start()
    gateway=Server(('127.0.0.1',0),Gateway);threading.Thread(target=gateway.serve_forever,daemon=True).start()
    with tempfile.TemporaryDirectory(prefix='flowdisk-seed-')as tmp:
        root=Path(tmp);os.environ['FLOWDISK_TEST_ROOT']=str(root)
        os.environ.pop('TRANSFER_ENGINE',None);os.environ['ARIA2_RPC_PORT']=str(free_port());os.environ['BT_PORT']=str(free_port())
        (root/'source').mkdir();(root/'data').mkdir();(root/'source'/'payload.bin').write_bytes(PAYLOAD)
        info={b'name':b'payload.bin',b'length':len(PAYLOAD),b'piece length':16384,b'private':1,b'pieces':b''.join(hashlib.sha1(PAYLOAD[i:i+16384]).digest()for i in range(0,len(PAYLOAD),16384))}
        torrent=bencode({b'announce':f'http://127.0.0.1:{tracker.server_port}/announce'.encode(),b'info':info})
        (root/'fixture.torrent').write_bytes(torrent)
        source_log=(root/'source.log').open('w')
        source=subprocess.Popen([aria,'--quiet=true','--enable-dht=false','--enable-dht6=false','--enable-peer-exchange=false','--disable-ipv6=true','--file-allocation=none','--check-integrity=true','--seed-ratio=0','--seed-time=60','--bt-tracker-interval=1','--listen-port='+str(source_port),'--dir='+str(root/'source'),str(root/'fixture.torrent')],stdout=source_log,stderr=subprocess.STDOUT)
        app=None
        try:
            app=App(binary,root,gateway)
            task=app.api('/transfers',{'torrent_base64':base64.b64encode(torrent).decode(),'name':'fixture.torrent','create_folder':False})
            ident=task['id']
            for _ in range(100):
                row=next(t for t in app.api('/transfers')if t['id']==ident)
                if row['files']:break
                time.sleep(.1)
            app.api('/transfers/'+ident+'/select',{'indices':[1]})
            wait(app,ident,lambda t:t['status']=='seeding')
            payload=root/'downloads'/'payload.bin';assert payload.read_bytes()==PAYLOAD
            inode=payload.stat().st_ino
            app.api('/transfers/'+ident+'/seed',{'enabled':False})
            assert payload.stat().st_ino==inode
            app.api('/local/operation',{'action':'mkdir','path':'/','name':'archive'})
            app.api('/local/operation',{'action':'move','paths':['/payload.bin'],'destination':'/archive'})
            app.api('/local/operation',{'action':'rename','paths':['/archive/payload.bin'],'name':'renamed.bin'})
            payload=root/'downloads'/'archive'/'renamed.bin'
            before=payload.stat()
            app.api('/transfers/'+ident+'/seed',{'enabled':True})
            wait(app,ident,lambda t:t['status']=='seeding')
            assert payload.stat().st_ino==inode and payload.stat().st_mtime_ns==before.st_mtime_ns
            leech(int(app.env['BT_PORT']),hashlib.sha1(bencode(info)).digest())
            wait(app,ident,lambda t:t['uploaded']>=16384)
            app.api('/transfers/'+ident+'/seed',{'enabled':False})
            # Invalid contents of the same length must not get silently re-downloaded.
            damaged=b'!'+PAYLOAD[1:];payload.write_bytes(damaged);bad_time=payload.stat().st_mtime_ns
            app.api('/transfers/'+ident+'/seed',{'enabled':True})
            for _ in range(200):
                row=next(t for t in app.api('/seeding')if t['id']==ident)
                if row['error']:break
                time.sleep(.1)
            else:raise AssertionError('bad payload accepted')
            assert not row['enabled'] and payload.read_bytes()==damaged and payload.stat().st_mtime_ns==bad_time,row
            payload.write_bytes(PAYLOAD)
            app.api('/transfers/'+ident+'/seed',{'enabled':True});wait(app,ident,lambda t:t['status']=='seeding')
            app.stop();app.start()
            row=next(t for t in app.api('/seeding')if t['id']==ident);assert not row['enabled']
            app.api('/transfers/'+ident+'/seed',{'enabled':True});wait(app,ident,lambda t:t['status']=='seeding')
            app.api('/transfers/'+ident+'/seed',{'enabled':False,'forget':True})
            assert not app.api('/seeding') and payload.read_bytes()==PAYLOAD and payload.stat().st_ino==inode
            entries=app.api('/local/tree?path='+quote('/archive'))['entries'];assert not entries[0]['seeding'] and not entries[0]['locked']
            # Idle records no longer rewrite the full torrent metadata every second.
            state=root/'data'/'transfers.json';saved=state.stat().st_mtime_ns;time.sleep(1.3);assert state.stat().st_mtime_ns==saved
            print('PASS real BT download, moved-file hash validation and upload, stop/unlock, corruption refusal, restart pause, forget/preserve and idle writes',flush=True)
        except Exception:
            if app:
                print((root/'server.log').read_text()[-2000:],file=sys.stderr)
                print(json.dumps(app.api('/transfers')),file=sys.stderr)
            raise
        finally:
            if app and app.proc.poll()is None:app.stop()
            source.terminate();source.wait(timeout=15);source_log.close();tracker.shutdown();gateway.shutdown()

if __name__=='__main__':run(sys.argv[1])
