#!/usr/bin/env python3
"""Real aria2 concurrency, uppercase proxy and resume checks, without external traffic.
Run: python3 tests/aria2_download.py target/debug/flowdisk
"""
import hashlib
import os
from pathlib import Path
import re
import sys
import tempfile
import threading
import time
import urllib.request
from urllib.parse import urlsplit, parse_qs
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from control import App, Gateway, Server
from integration import free_port

PAYLOAD = bytes(range(256)) * (32 * 1024)  # 8 MiB; kept in memory/tmpfs when available.
LOCK = threading.Lock()
ACTIVE, PEAK, RANGES = {}, {}, {}


class Proxy(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, *_):
        pass

    def do_GET(self):
        url = urlsplit(self.path)
        case = parse_qs(url.query).get('case', ['unknown'])[0]
        if url.path == '/redirect':
            self.send_response(302)
            self.send_header('Location', 'http://flowdisk-cdn.invalid/payload?case=' + case)
            self.send_header('Content-Length', '0')
            self.end_headers()
            return
        assert url.hostname == 'flowdisk-cdn.invalid', self.path
        start, end = 0, len(PAYLOAD) - 1
        span = re.fullmatch(r'bytes=(\d+)-(\d*)', self.headers.get('Range', ''))
        ranged = span is not None and case != 'no-range'
        if ranged:
            start = int(span[1])
            end = min(int(span[2]) if span[2] else end, end)
        self.send_response(206 if ranged else 200)
        self.send_header('Content-Length', str(end - start + 1))
        self.send_header('ETag', '"unchanged-fixture"')
        if ranged:
            self.send_header('Content-Range', f'bytes {start}-{end}/{len(PAYLOAD)}')
        self.end_headers()
        with LOCK:
            ACTIVE[case] = ACTIVE.get(case, 0) + 1
            PEAK[case] = max(PEAK.get(case, 0), ACTIVE[case])
            RANGES.setdefault(case, []).append(start)
        try:
            for offset in range(start, end + 1, 16384):
                self.wfile.write(PAYLOAD[offset:min(offset + 16384, end + 1)])
                self.wfile.flush()
                time.sleep(.012)
        except (BrokenPipeError, ConnectionResetError):
            pass
        finally:
            with LOCK:
                ACTIVE[case] -= 1


def wait(app, ident, ready, timeout=45):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        row = next(t for t in app.api('/transfers') if t['id'] == ident)
        assert row['status'] != 'failed', row.get('error')
        if ready(row):
            return row
        time.sleep(.1)
    raise AssertionError(('timeout', row))


def verify(app, row):
    index = row['files'][0]['index']
    req = urllib.request.Request(app.base + f'/api/transfers/{row["id"]}/files/{index}', headers={'Authorization': 'Bearer ' + app.env['APP_TOKEN']})
    with urllib.request.urlopen(req) as response:
        assert hashlib.sha256(response.read()).digest() == hashlib.sha256(PAYLOAD).digest()


def main():
    for key in list(os.environ):
        if 'proxy' in key.lower() or key == 'TRANSFER_ENGINE':
            os.environ.pop(key)
    proxy = ThreadingHTTPServer(('127.0.0.1', 0), Proxy)
    proxy.daemon_threads = True
    gateway = Server(('127.0.0.1', 0), Gateway)
    for server in [proxy, gateway]:
        threading.Thread(target=server.serve_forever, daemon=True).start()
    # Only uppercase proxy variables, as supplied by the production Compose file.
    os.environ.update(HTTP_PROXY=f'http://127.0.0.1:{proxy.server_port}', HTTPS_PROXY=f'http://127.0.0.1:{proxy.server_port}', NO_PROXY='127.0.0.1,localhost', LINK_CONNECTIONS_PER_FILE='1', LINK_MIN_SPLIT_MIB='1', ARIA2_RPC_PORT=str(free_port()), BT_PORT=str(free_port()))
    with tempfile.TemporaryDirectory(prefix='flowdisk-aria2-', dir='/dev/shm' if Path('/dev/shm').is_dir() else None) as tmp:
        app = App(sys.argv[1], Path(tmp), gateway)
        try:
            def create(case):
                return app.api('/transfers', {'uri': 'http://flowdisk-download.invalid/redirect?case=' + case, 'name': case + '.bin', 'create_folder': False})['id']

            times = {}
            for connections, case in [(1, 'one'), (8, 'parallel')]:
                if connections == 8:
                    app.stop()
                    app.env['LINK_CONNECTIONS_PER_FILE'] = '8'
                    app.start()
                status = app.api('/transfers/status')
                assert status['connections_per_file'] == connections and status['min_split_mib'] == 1
                started = time.monotonic()
                row = wait(app, create(case), lambda t: t['status'] == 'completed')
                times[case] = round(time.monotonic() - started, 2)
                verify(app, row)
            assert PEAK['one'] == 1 and PEAK['parallel'] >= 4, PEAK
            print('PASS real aria2 redirected proxy download: peak connections', PEAK, 'elapsed seconds', times, flush=True)

            app.stop()
            app.env['LINK_CONNECTIONS_PER_FILE'] = '1'
            app.start()
            ident = create('resume')
            wait(app, ident, lambda t: 0 < t['downloaded'] < t['size'])
            app.api('/transfers/' + ident + '/pause', {})
            wait(app, ident, lambda t: t['status'] == 'paused')
            app.stop()
            app.env['LINK_CONNECTIONS_PER_FILE'] = '8'
            app.start()
            prior = len(RANGES['resume'])
            app.api('/transfers/' + ident + '/resume', {})
            row = wait(app, ident, lambda t: t['status'] == 'completed')
            assert any(start > 0 for start in RANGES['resume'][prior:]), RANGES['resume'][prior:]
            verify(app, row)
            assert PEAK['resume'] >= 4, PEAK
            print('PASS pause/restart/resume keeps partial ranges and byte-identical output', flush=True)

            row = wait(app, create('no-range'), lambda t: t['status'] == 'completed')
            verify(app, row)
            print('PASS source without Range falls back to a complete valid download', flush=True)
        finally:
            app.stop()
            app.log.close()
            for server in [proxy, gateway]:
                server.shutdown()
                server.server_close()


if __name__ == '__main__':
    main()
