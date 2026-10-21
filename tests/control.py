#!/usr/bin/env python3
"""Account/share/file API checks against an in-memory OpenList, no real SMS or account."""
import json
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
import uuid
from pathlib import Path
from integration import App, Fixture, Server, PAYLOAD

STORAGES=[]
TREE={'/quark':None,'/quark/资料':None,'/quark/资料/a.bin':0,'/quark/资料/子目录':None,'/quark/资料/子目录/b.bin':0,'/quark/slow.bin':len(PAYLOAD)}
OPS=[]
PREVIEW_OFFLINE=False
class Gateway(Fixture):
    def do_PUT(self):
        from urllib.parse import unquote
        assert self.path=='/api/fs/put'
        assert self.headers['Overwrite']=='false' and self.headers['As-Task']=='false'
        path=unquote(self.headers['File-Path'])
        data=self.rfile.read(int(self.headers['Content-Length']))
        if path in TREE:return self.send_json({'code':403,'data':None})
        TREE[path]=len(data)
        return self.send_json({'code':200,'data':{}})
    def do_GET(self):
        if self.path.startswith('/preview-fixture/'):
            if PREVIEW_OFFLINE:self.send_error(500);return
            data=b'<script>untrusted-text</script>\npreview-content-1234567890';size=len(data)
            span=self.headers.get('Range');start,end=0,size-1
            if span:
                start,last=span.removeprefix('bytes=').split('-');start=int(start);end=min(int(last)if last else size-1,size-1)
            self.send_response(206 if span else 200)
            if span:self.send_header('Content-Range',f'bytes {start}-{end}/{size}')
            self.send_header('Content-Length',str(end-start+1));self.end_headers();self.wfile.write(data[start:end+1]);return
        if self.path.startswith('/api/admin/storage/list'):
            return self.send_json({'code':200,'data':{'content':STORAGES}})
        if self.path.startswith('/api/admin/driver/info'):
            return self.send_json({'code':200,'data':{'additional':[{'name':'root_folder_id','type':'string','default':'0'}]}})
        return super().do_GET()
    def do_POST(self):
        if not self.path.startswith(('/api/admin/storage/','/api/fs/')): return super().do_POST()
        b=json.loads(self.rfile.read(int(self.headers.get('Content-Length',0))) or '{}')
        route=self.path
        if route.startswith('/api/admin/storage/delete'):
            sid=int(route.split('id=')[1]);STORAGES[:]=[s for s in STORAGES if s['id']!=sid];v={}
        elif route in ['/api/admin/storage/create','/api/admin/storage/update']:
            if route.endswith('create'): b['id']=len(STORAGES)+1
            STORAGES[:]=[s for s in STORAGES if s['id']!=b['id']]
            b['status']='work';STORAGES.append(b);v=b
            root=b['mount_path'];TREE.setdefault(root,None)
            if b['driver']=='115 Share':TREE[root+'/分享测试.bin']=0
        elif route=='/api/fs/list':
            p=b['path'];names=[]
            if any(p==s['mount_path'] and 'reject-me' in s['addition'] for s in STORAGES):return self.send_json({'code':403,'message':'invalid authorization'})
            for path,size in TREE.items():
                parent,_,name=path.rpartition('/')
                if parent==p.rstrip('/'):names.append({'name':name,'is_dir':size is None,'size':size or 0,'modified':'2026-10-19T00:00:00Z'})
            if p=='/':names=[{'name':s['mount_path'].lstrip('/'),'size':0,'is_dir':True} for s in STORAGES if s['mount_path'].count('/')==1]
            v={'content':names,'total':len(names)}
        elif route=='/api/fs/get':
            p=b['path']
            if p not in TREE:return self.send_json({'code':404,'message':'not found'})
            size=TREE[p];v={'name':p.rsplit('/',1)[-1],'size':size or 0,'is_dir':size is None,'modified':'2026-10-19T00:00:00Z','raw_url':f'http://127.0.0.1:{self.server.server_port}/cdn/'+('ranged' if p.endswith('/slow.bin') else 'empty')}
            if '/preview-test/'in p and size:v['raw_url']=f'http://127.0.0.1:{self.server.server_port}/preview-fixture/file'
        elif route=='/api/fs/link':
            v={'url':f'http://127.0.0.1:{self.server.server_port}/cdn/'+('ranged' if b['path'].endswith('/slow.bin') else 'empty'),'header':{'X-Provider-Auth':['private-cookie'],'User-Agent':['pan.baidu.com']}}
            if '/preview-test/'in b['path'] and TREE[b['path']]:v['url']=f'http://127.0.0.1:{self.server.server_port}/preview-fixture/file'
        elif route=='/api/fs/mkdir':TREE[b['path']]=None;v={};OPS.append((route,b))
        elif route=='/api/fs/rename':
            p=b['path'];target=p.rsplit('/',1)[0]+'/'+b['name']
            for source in list(TREE):
                if source==p or source.startswith(p+'/'):TREE[target+source[len(p):]]=TREE.pop(source)
            v={};OPS.append((route,b))
        elif route=='/api/fs/remove':
            for n in b['names']:
                p=b['dir']+'/'+n
                for source in list(TREE):
                    if source==p or source.startswith(p+'/'):TREE.pop(source)
            v={};OPS.append((route,b))
        elif route=='/api/fs/move':v={'task':{'id':'mock-move'}};OPS.append((route,b))
        else:return self.send_json({'code':500,'message':'unexpected endpoint'})
        self.send_json({'code':200,'data':v})

def reject(app,path,body=None,method=None,code=400,auth=True):
    try:app.api(path,body,method,auth);raise AssertionError('unexpected acceptance: '+path)
    except urllib.error.HTTPError as e:assert e.code==code,(e.code,e.read())
def wait_share(app,sid):
    for _ in range(120):
        s=next(s for s in app.api('/shares') if s['id']==sid)
        if s['status']=='ready':return s
        assert s['status']!='failed',s
        time.sleep(.05)
    raise AssertionError(s)
def main():
    server=Server(('127.0.0.1',0),Gateway);threading.Thread(target=server.serve_forever,daemon=True).start()
    with tempfile.TemporaryDirectory(prefix='flowdisk-controls-') as tmp:
        root=Path(tmp);app=App(sys.argv[1],root,server)
        try:
            for path in ['/accounts','/shares','/local']:reject(app,path,code=401,auth=False)
            accounts=app.api('/accounts');assert accounts[1]['methods'][0]=='sms';assert 'sms' not in accounts[2]['methods'];assert all(not a['connected'] for a in accounts)
            assert {a['id'] for a in accounts}=={'baidu','quark','115','thunder','onedrive','google'}
            for provider in ['onedrive','google']:
                session=app.api('/accounts/'+provider+'/login',{'mode':'oauth'})
                assert session.get('setup_required') and session['helper_url']=='https://api.oplist.org/'
                reject(app,f'/logins/{session["id"]}/action',{'secret':'private-refresh'})
                # Callback is state-bound even without an app credential configured.
                try:urllib.request.urlopen(app.base+'/oauth/'+('google' if provider=='onedrive' else 'onedrive')+'/callback?state='+session['id']+'&code=untrusted')
                except urllib.error.HTTPError as error:assert error.code==400
                else:raise AssertionError('cross-provider OAuth state accepted')
                app.api('/logins/'+session['id'],method='DELETE')
                session=app.api('/accounts/'+provider+'/login',{'mode':'credential'})
                reject(app,f'/logins/{session["id"]}/action',{'secret':'private-refresh','source':'custom'})
                if provider=='google':reject(app,f'/logins/{session["id"]}/action',{'secret':'/etc/passwd'})
                connected=app.api(f'/logins/{session["id"]}/action',{'secret':'1//fixture_refresh' if provider=='google' else 'private-refresh'})
                assert connected['status']=='connected' and 'private-refresh' not in json.dumps(connected)
                mounted=next(x for x in STORAGES if x['mount_path']=='/'+provider)
                addition=json.loads(mounted['addition']);assert addition['use_online_api'] is True
                assert addition['root_folder_id' if provider=='google' else 'root_folder_path']==('root' if provider=='google' else '/')
                app.api('/accounts/'+provider,method='DELETE')
            s=app.api('/accounts/quark/login',{'mode':'sms'});assert s['form_url'].startswith('https://uop.quark.cn/');assert 'qr_svg' not in s
            reject(app,f'/logins/{s["id"]}/input',{'kind':'click'},code=404)
            reject(app,f'/logins/{s["id"]}/action',{'ticket':'bad'})
            app.api(f'/logins/{s["id"]}',method='DELETE');reject(app,f'/logins/{s["id"]}')
            reject(app,'/accounts/115/login',{'mode':'sms'});reject(app,'/accounts/evil/login',{'mode':'credential'})
            reject(app,'/shares',{'url':'https://127.0.0.1/s/x'})
            share=app.api('/shares',{'url':'https://115.com/s/abc?password=1234'})
            sid=share['id'];assert share['status']=='waiting_login';assert 'passcode' not in share
            assert app.api('/shares',{'url':'https://115.com/s/abc?password=1234'})['id']==sid
            app.stop();app.start();assert app.api('/shares')[0]['id']==sid
            print('PASS native SMS session, capability checks, no input relay, share validation/persistence',flush=True)
            login=app.api('/accounts/115/login',{'mode':'credential'})
            result=app.api('/logins/'+login['id']+'/action',{'secret':'UID=test; CID=test; SEID=private-session-cookie'})
            assert result['status']=='connected';assert 'private-session-cookie' not in json.dumps(result)
            assert 'private-session-cookie' not in json.dumps(app.api('/accounts'))
            s=wait_share(app,sid);assert not s['jobs'] and not app.api('/jobs')
            content=app.api('/shares/'+sid+'/contents');assert len(content['entries'])==2
            ids=app.api('/shares/'+sid+'/download',{'paths':content['roots']})['jobs'];assert len(ids)==1
            s=next(s for s in app.api('/shares')if s['id']==sid);assert len(s['jobs'])==1
            job=app.wait(s['jobs'][0]);assert job['status']=='completed';assert any(a['connected'] for a in app.api('/accounts'))
            assert app.job(s['jobs'][0])['group_id']==sid
            print('PASS login prepares share without downloads; explicit selection creates grouped download',flush=True)
            local=app.api('/local');assert len(local)==1
            file_id=local[0]['id'];reject(app,f'/local/{file_id}/rename',{'name':'../x'});reject(app,f'/local/{file_id}/rename',{'name':' bad.bin'})
            app.api(f'/local/{file_id}/rename',{'name':'下载完成.bin'});assert app.job(file_id)['name']=='下载完成.bin'
            app.api('/jobs/'+file_id,method='DELETE');assert app.api('/local')[0]['name']=='下载完成.bin'
            app.stop();app.start();assert app.api('/local')[0]['id']==file_id
            app.api('/local/'+file_id,method='DELETE');assert not app.api('/local')
            orphan=str(uuid.uuid4());(root/'downloads'/orphan).mkdir();(root/'downloads'/orphan/'保留.txt').write_text('kept')
            link=str(uuid.uuid4());(root/'downloads'/link).symlink_to(root/'downloads'/orphan,target_is_directory=True)
            linked_file=str(uuid.uuid4());(root/'downloads'/linked_file).mkdir();(root/'downloads'/linked_file/'链接.txt').symlink_to(root/'downloads'/orphan/'保留.txt')
            assert [f['id'] for f in app.api('/local')]==[orphan]
            print('PASS local rename/history removal/restart/delete, orphan discovery and symlink exclusion',flush=True)
            # One controlled flow covers preview MIME/Range, selection boundaries,
            # ad acknowledgement, password notes and capacity approval guards.
            mount=content['roots'][0];folder=mount+'/preview-test';TREE[folder]=None
            preview_bytes=b'<script>untrusted-text</script>\npreview-content-1234567890'
            for name in ['sample.txt','sample.html','sample.png','sample.mp3','sample.mp4']:TREE[folder+'/'+name]=len(preview_bytes)
            TREE[folder+'/网站.url']=0;TREE[folder+'/资料.zip']=0;TREE[folder+'/解压密码：abc123.txt']=0
            app.api('/shares/'+sid+'/retry',{});wait_share(app,sid)
            content=app.api('/shares/'+sid+'/contents');assert not app.api('/jobs')
            assert next(e for e in content['entries']if e['name']=='资料.zip')['passwords']==['abc123']
            assert next(e for e in content['entries']if e['name']=='网站.url')['ad_reason']
            reject(app,'/shares/'+sid+'/download',{'paths':['/quark/slow.bin']})
            reject(app,'/shares/'+sid+'/download',{'paths':[folder]})
            from urllib.parse import urlencode
            for name,mime in [('sample.txt','text/plain'),('sample.html','text/plain'),('sample.png','image/png'),('sample.mp3','audio/mpeg'),('sample.mp4','video/mp4')]:
                req=urllib.request.Request(app.base+'/api/shares/'+sid+'/preview?'+urlencode({'path':folder+'/'+name}),headers={'Authorization':'Bearer '+app.env['APP_TOKEN'],'Range':'bytes=4-10'})
                with urllib.request.urlopen(req)as response:
                    assert response.status==206 and response.headers['Content-Type'].startswith(mime)
                    assert response.read()==preview_bytes[4:11]
                    assert 'private-cookie'not in str(response.headers)
            reject(app,'/shares/'+sid+'/preview?'+urlencode({'path':'/quark/slow.bin'}))
            req=urllib.request.Request(app.base+'/api/shares/'+sid+'/preview?'+urlencode({'path':folder+'/sample.png'}),headers={'Authorization':'Bearer '+app.env['APP_TOKEN']})
            with urllib.request.urlopen(req)as response:assert response.status==200 and response.read()==preview_bytes
            ids=app.api('/shares/'+sid+'/download',{'paths':[folder+'/sample.txt',folder+'/资料.zip']})['jobs']
            for jid in ids:app.wait(jid)
            assert len(ids)==2 and {app.job(j)['group_id']for j in ids}=={sid}
            assert next(app.job(j)for j in ids if app.job(j)['name']=='资料.zip')['passwords']==['abc123']
            globals()['PREVIEW_OFFLINE']=True
            req=urllib.request.Request(app.base+'/api/shares/'+sid+'/preview?'+urlencode({'path':folder+'/sample.txt'}),headers={'Authorization':'Bearer '+app.env['APP_TOKEN'],'Range':'bytes=4-10'})
            with urllib.request.urlopen(req)as response:assert response.status==206 and response.read()==preview_bytes[4:11]
            globals()['PREVIEW_OFFLINE']=False
            app.stop();records_path=root/'data'/'shares.json';records=json.loads(records_path.read_text());records[sid]['status']='insufficient_capacity';records[sid]['capacity']={'known':True,'enough':False,'total':100,'used':90,'free':10,'required':20,'after':0};records_path.write_text(json.dumps(records));app.start()
            reject(app,'/shares/'+sid+'/prepare',{})
            app.stop();records[sid]['status']='awaiting_transfer';records[sid]['capacity']['enough']=True;records_path.write_text(json.dumps(records));app.start()
            app.api('/shares/'+sid+'/prepare',{});wait_share(app,sid)
            assert app.job(ids[0])['group_id']==sid
            for jid in ids:app.api('/jobs/'+jid,method='DELETE')
            print('PASS previews for four media types, safe HTML text, scoped selection, ads, passwords, group persistence and capacity gate',flush=True)
            login=app.api('/accounts/quark/login',{'mode':'credential'});app.api('/logins/'+login['id']+'/action',{'secret':'__pus=fake-quark-auth-cookie'})
            invalid=app.api('/accounts/quark/login',{'mode':'credential'})
            reject(app,'/logins/'+invalid['id']+'/action',{'secret':'reject-me-cookie'})
            assert app.api('/logins/'+invalid['id'])['status']=='waiting'
            assert 'reject-me' not in next(s['addition'] for s in STORAGES if s['driver']=='Quark')
            assert app.api('/logins/'+invalid['id']+'/action',{'secret':'__pus=replacement-fake-quark-cookie'})['status']=='connected'
            active=app.api('/jobs',{'path':'/quark/slow.bin'})['id']
            reject(app,'/files/operation',{'action':'rename','path':'/quark/slow.bin','name':'busy.bin'})
            reject(app,'/accounts/quark',method='DELETE')
            reject(app,'/local/'+active,method='DELETE')
            reject(app,'/local/'+active+'/rename',{'name':'busy.bin'})
            app.api('/jobs/'+active+'/pause',{},'POST');app.wait(active,'paused');app.api('/jobs/'+active,method='DELETE')
            jobs=app.api('/batch-download',{'paths':['/quark/资料']})['jobs'];assert len(jobs)==2
            for jid in jobs:app.wait(jid)
            assert app.api('/batch-download',{'paths':['/quark/资料']})['jobs']==jobs
            op=lambda **b:app.api('/files/operation',b)
            op(action='mkdir',path='/quark',name='新目录');assert '/quark/新目录' in TREE
            from urllib.parse import quote
            upload_url=app.base+'/api/files/upload?path='+quote('/quark/新目录/网页上传.txt')
            request=urllib.request.Request(upload_url,data=b'upload-test',method='PUT',headers={'Authorization':'Bearer '+app.env['APP_TOKEN'],'Content-Type':'application/octet-stream'})
            with urllib.request.urlopen(request)as response:assert json.load(response)['ok']
            assert TREE['/quark/新目录/网页上传.txt']==11
            try:urllib.request.urlopen(request);raise AssertionError('upload overwrote existing file')
            except urllib.error.HTTPError as e:assert e.code==502

            op(action='rename',path='/quark/资料',name='重命名');assert all(j['path'].startswith('/quark/重命名/') for j in app.api('/jobs'))
            reject(app,'/files/operation',{'action':'rename','path':'/quark','name':'root'})
            reject(app,'/files/operation',{'action':'mkdir','path':'/quark','name':'../x'})
            reject(app,'/files/operation',{'action':'move','path':'/quark','names':['重命名'],'destination':'/115'})
            reject(app,'/files/operation',{'action':'move','path':'/quark','names':['重命名'],'destination':'/quark/重命名/子目录'})
            moved=op(action='move',path='/quark',names=['重命名'],destination='/quark/新目录');assert moved['submitted']
            op(action='delete',path='/quark',names=['新目录']);assert '/quark/新目录' not in TREE
            # Real Thunder shares can contain DEL in existing names. Preserve remote
            # lookup in JSON, sanitize only the local output, and keep new names strict.
            odd='/quark/奇名目录/\x7fremote.txt';TREE['/quark/奇名目录']=None;TREE[odd]=0
            odd_job=app.api('/batch-download',{'paths':['/quark/奇名目录']})['jobs'][0]
            app.wait(odd_job);assert app.job(odd_job)['path']==odd and app.job(odd_job)['name']=='_remote.txt'
            reject(app,'/files/operation',{'action':'mkdir','path':'/quark','name':'\x7fnew'})
            op(action='delete',path='/quark/奇名目录',names=['\x7fremote.txt']);assert odd not in TREE
            print('PASS remote control-character name: recursive download, safe local name and cloud deletion',flush=True)
            app.api('/accounts/115',method='DELETE');assert all(s['driver']!='115 Share' and s['driver']!='115 Cloud' for s in STORAGES)
            print('PASS recursive/batch download deduplication, cloud mkdir/rename/move/delete guards, logout clears share mounts',flush=True)
            app.stop();app.env.update(BAIDU_CLIENT_ID='test-client',BAIDU_CLIENT_SECRET='private-oauth-secret',PUBLIC_URL=app.base);app.start()
            oauth=app.api('/accounts/baidu/login',{'mode':'oauth'})
            assert oauth['authorize_url'].startswith('https://openapi.baidu.com/oauth/2.0/authorize?')
            assert oauth['id'] in oauth['authorize_url'] and 'private-oauth-secret' not in json.dumps(oauth)
            try: urllib.request.urlopen(app.base+'/oauth/baidu/callback?state='+str(uuid.uuid4())+'&error=access_denied');raise AssertionError('invalid OAuth state accepted')
            except urllib.error.HTTPError as error:assert error.code==400
            try: urllib.request.urlopen(app.base+'/oauth/baidu/callback?state='+oauth['id']+'&error=access_denied')
            except urllib.error.HTTPError as error:assert error.code==400
            assert app.api('/logins/'+oauth['id'])['status']=='cancelled'
            print('PASS rejected credentials roll back and can be corrected; OAuth state/cancellation and secret redaction',flush=True)
            print('ALL CONTROL CHECKS PASSED',flush=True)
        finally:
            if app.proc.poll() is None:app.stop()
            server.shutdown()
if __name__=='__main__':main()
