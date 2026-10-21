#!/usr/bin/env python3
"""Focused local-folder check. Run with a built binary; ARIA2_BIN optionally checks real links."""
import hashlib
import json
import os
from pathlib import Path
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from urllib.parse import quote
import uuid
from control import App, Gateway, Server, TREE
from integration import TOKEN, free_port

DATA = b'<script>untrusted-text</script>\npreview-content-1234567890'


def reject(app, path, body=None, code=None):
    try:
        app.api(path, body)
        raise AssertionError(('accepted invalid operation', path, body))
    except urllib.error.HTTPError as error:
        assert error.code == code if code else error.code in (400, 404, 409, 500), error.code


def run(binary):
    os.environ.setdefault('TRANSFER_ENGINE', 'disabled' if not os.environ.get('ARIA2_BIN') else 'enabled')
    os.environ['ARIA2_RPC_PORT'] = str(free_port())
    os.environ['BT_PORT'] = str(free_port())
    server = Server(('127.0.0.1', 0), Gateway)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    with tempfile.TemporaryDirectory(prefix='flowdisk-folders-') as tmp:
        root = Path(tmp)
        app = App(binary, root, server)
        downloads = root/'downloads'
        try:
            for folder in ('pack', 'flat'):
                base = '/quark/preview-test/'+folder
                TREE[base] = None
                TREE[base+'/a.txt'] = len(DATA)
                TREE[base+'/sub'] = None
                TREE[base+'/sub/b.txt'] = len(DATA)
            def tree(path='/'):
                return app.api('/local/tree?path='+quote(path))['entries']
            def op(action, **args):
                return app.api('/local/operation', dict(action=action, **args))
            def get(path, span=None):
                headers = {'Authorization':'Bearer '+TOKEN}
                if span:
                    headers['Range'] = span
                with urllib.request.urlopen(urllib.request.Request(app.base+'/api'+path, headers=headers)) as response:
                    return response.status, response.read()
            def job_path(jid):
                job = app.job(jid)
                return downloads/(job['local_dir'] if job['local_dir'] is not None else jid)/job['name']

            TREE['/quark/preview-test/legacy.txt'] = len(DATA)
            legacy = app.api('/jobs', {'path':'/quark/preview-test/legacy.txt'})['id']
            app.wait(legacy)
            app.api('/local/'+legacy+'/rename', {'name':'legacy-renamed.txt'})
            assert len(app.api('/local'))==1
            assert get('/local/'+legacy+'/file')==(200,DATA)
            app.api('/jobs/'+legacy,method='DELETE')
            assert app.api('/local')[0]['name']=='legacy-renamed.txt'
            app.api('/local/'+legacy+'/rename', {'name':'orphan-renamed.txt'})
            assert get('/local/'+legacy+'/file')==(200,DATA)
            app.api('/local/'+legacy,method='DELETE')
            assert not app.api('/local')
            ids = app.api('/batch-download', {'paths':['/quark/preview-test/pack']})['jobs']
            for jid in ids:
                app.wait(jid)
            files = [job_path(jid) for jid in ids]
            batch = {p.relative_to(downloads).parts[0] for p in files}
            assert len(batch) == 1 and len(ids) == 2, files
            assert all(p.read_bytes() == DATA for p in files)
            inode = {jid:job_path(jid).stat().st_ino for jid in ids}
            op('mkdir', path='/', name='整理')
            op('mkdir', path='/', name='其他')
            folder = '/'+batch.pop()
            result = op('move', paths=[folder], destination='/整理')
            assert result['method'] == 'rename' and result['copied_bytes'] == 0
            for jid in ids:
                assert job_path(jid).stat().st_ino == inode[jid]
                assert get('/jobs/'+jid+'/file', 'bytes=4-14') == (206, DATA[4:15])
            first = ids[0]
            relative = '/'+str(job_path(first).relative_to(downloads))
            op('move', paths=[relative], destination='/其他')
            assert job_path(first).stat().st_ino == inode[first]
            op('rename', paths=['/其他/'+app.job(first)['name']], name='新名字.txt')
            assert app.job(first)['name'] == '新名字.txt'
            assert get('/local/tree/file?path='+quote('/其他/新名字.txt')) == (200,DATA)
            op('mkdir', path='/整理', name='空目录')
            op('move', paths=['/整理/空目录'], destination='/其他')
            assert any(e['name']=='空目录' for e in tree('/其他'))
            # Preflight conflicts and cycles leave the source and target intact.
            (downloads/'其他'/'conflict.txt').write_bytes(b'destination')
            (downloads/'conflict.txt').write_bytes(b'source')
            reject(app, '/local/operation', {'action':'move','paths':['/conflict.txt'],'destination':'/其他'},400)
            assert (downloads/'conflict.txt').read_bytes()==b'source'
            assert (downloads/'其他'/'conflict.txt').read_bytes()==b'destination'
            reject(app, '/local/operation', {'action':'move','paths':['/整理'],'destination':'/整理/'+folder.lstrip('/')},400)
            reject(app, '/local/operation', {'action':'move','paths':['/其他','/其他/新名字.txt'],'destination':'/'},400)
            reject(app, '/local/operation', {'action':'mkdir','path':'/','name':'../outside'},400)
            reject(app, '/local/tree?path=/../outside',code=400)
            (root/'outside').mkdir()
            (root/'outside'/'secret.txt').write_bytes(b'outside')
            (downloads/'symlink').symlink_to(root/'outside',target_is_directory=True)
            assert not any(e['name']=='symlink' for e in tree())
            reject(app, '/local/tree?path=/symlink')
            reject(app, '/local/tree/file?path=/symlink/secret.txt')
            # No outer batch folder when disabled; preserve nested source folders.
            (downloads/'a.txt').write_bytes(b'keep existing')
            flat = app.api('/batch-download', {'paths':['/quark/preview-test/flat'],'create_folder':False})['jobs']
            for jid in flat:
                app.wait(jid)
            assert {app.job(jid)['local_dir'] for jid in flat} == {'','sub'}
            assert (downloads/'a.txt').read_bytes()==b'keep existing'
            assert (downloads/'a (2).txt').read_bytes()==DATA
            # A folder with content only in a descendant still keeps that descendant.
            TREE['/quark/preview-test/nested-only']=None
            TREE['/quark/preview-test/nested-only/child']=None
            TREE['/quark/preview-test/nested-only/child/c.txt']=len(DATA)
            TREE['/quark/preview-test/nested-only/child/d.txt']=len(DATA)
            nested=app.api('/batch-download',{'paths':['/quark/preview-test/nested-only'],'create_folder':False})['jobs']
            for jid in nested:
                app.wait(jid)
                assert app.job(jid)['local_dir']=='child'
            # Unfinished output paths cannot move with their parent.
            slow = app.api('/jobs',{'path':'/quark/slow.bin'})['id']
            app.api('/jobs/'+slow+'/pause',{},'POST');app.wait(slow,'paused')
            (downloads/slow).mkdir(exist_ok=True)
            reject(app,'/local/operation',{'action':'move','paths':['/'+slow],'destination':'/其他'},400)
            app.api('/jobs/'+slow,method='DELETE')
            app.stop();app.start()
            for jid in ids+flat:
                assert get('/jobs/'+jid+'/file')==(200,DATA)
            # Recover the crash window after the rename and before saving task references.
            old = job_path(first).relative_to(downloads).as_posix()
            app.stop()
            new = '整理/恢复后.txt'
            before = (downloads/old).stat().st_ino
            os.rename(downloads/old,downloads/new)
            (root/'data'/'local-move.json').write_text(json.dumps({'moves':[[old,new]]}))
            app.start()
            assert job_path(first).stat().st_ino==before
            assert get('/jobs/'+first+'/file')==(200,DATA)
            assert not (root/'data'/'local-move.json').exists()
            app.api('/jobs/'+first,method='DELETE')
            assert get('/local/tree/file?path='+quote('/'+new))==(200,DATA)
            print('PASS native batch folder/opt-out, conflict-safe file and folder moves, inode preservation, Range, path guards, restart and journal recovery',flush=True)

            if os.environ.get('ARIA2_BIN'):
                assert app.api('/transfers/status')['available']
                group = str(uuid.uuid4())
                url = f'http://127.0.0.1:{server.server_port}/preview-fixture/file'
                link_ids=[]
                for name in ('one.txt','two.txt'):
                    link_ids.append(app.api('/transfers',dict(uri=url,name=name,group_id=group,group_name='链接测试'))['id'])
                flat_id = app.api('/transfers',dict(uri=url,name='flat-link.txt',create_folder=False))['id']
                deadline=time.monotonic()+45
                while True:
                    tasks={t['id']:t for t in app.api('/transfers')}
                    if all(tasks[jid]['status']=='completed' for jid in link_ids+[flat_id]):break
                    assert time.monotonic()<deadline,tasks
                    assert not any(t['status']=='failed' for t in tasks.values()),tasks
                    time.sleep(.2)
                dirs=[p for p in downloads.iterdir() if p.is_dir() and p.name.startswith('链接测试-')]
                assert len(dirs)==1 and (dirs[0]/'one.txt').read_bytes()==DATA and (dirs[0]/'two.txt').read_bytes()==DATA
                assert (downloads/'flat-link.txt').read_bytes()==DATA
                link_inode=(dirs[0]/'one.txt').stat().st_ino
                op('move',paths=['/'+dirs[0].name],destination='/整理')
                assert get('/transfers/'+link_ids[0]+'/files/1')==(200,DATA)
                target=downloads/'整理'/dirs[0].name
                assert (target/'one.txt').stat().st_ino==link_inode
                op('rename',paths=['/'+(target/'one.txt').relative_to(downloads).as_posix()],name='renamed-link.txt')
                app.stop();app.start()
                assert get('/transfers/'+link_ids[0]+'/files/1','bytes=1-9')==(206,DATA[1:10])
                assert (target/'renamed-link.txt').stat().st_ino==link_inode
                # A completed BT payload paused before stopping seeding remains readable,
                # and cannot be moved until its engine task is removed.
                app.stop()
                records=json.loads((root/'data'/'transfers.json').read_text())
                records[flat_id]['kind']='torrent';records[flat_id]['status']='paused'
                (root/'data'/'transfers.json').write_text(json.dumps(records))
                app.env['TRANSFER_ENGINE']='disabled';app.start()
                entry=next(e for e in tree() if e['name']=='flat-link.txt')
                assert entry['locked'] and entry['download_url']
                assert get(entry['download_url'].removeprefix('/api'))==(200,DATA)
                reject(app,'/local/operation',{'action':'move','paths':['/flat-link.txt'],'destination':'/其他'},400)
                app.api('/transfers/'+flat_id,method='DELETE')
                op('move',paths=['/flat-link.txt'],destination='/其他')
                assert get('/transfers/'+flat_id+'/files/1')==(200,DATA)
                print('PASS real aria2 batch/flat output, completed folder/file rename, stable task links and restart Range',flush=True)
        finally:
            if app.proc.poll() is None:app.stop()
            server.shutdown()

if __name__=='__main__':
    run(sys.argv[1])
