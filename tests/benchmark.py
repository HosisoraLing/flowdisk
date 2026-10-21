#!/usr/bin/env python3
"""Controlled local benchmark: one stream vs eight; not a real cloud speed claim."""
import hashlib
import json
import statistics
import sys
import tempfile
import threading
import time
from pathlib import Path
import integration as fixture

fixture.PAYLOAD = bytes(range(256)) * (512 * 1024)  # 128 MiB
fixture.SIZES['ranged'] = len(fixture.PAYLOAD)
expected = hashlib.sha256(fixture.PAYLOAD).hexdigest()
server = fixture.Server(('127.0.0.1', 0), fixture.Fixture)
threading.Thread(target=server.serve_forever, daemon=True).start()
results = []
try:
    with tempfile.TemporaryDirectory(prefix='flowdisk-bench-') as tmp:
        app = fixture.App(sys.argv[1] if len(sys.argv) > 1 else 'target/debug/flowdisk', Path(tmp), server)
        try:
            for connections in [1, 8]:
                app.stop()
                app.env['CONNECTIONS_PER_FILE'] = str(connections)
                app.start()
                samples = []
                for _ in range(3):
                    start = time.monotonic()
                    job = app.wait(app.create('ranged'))
                    elapsed = time.monotonic() - start
                    assert job['sha256'] == expected
                    samples.append(elapsed)
                median = statistics.median(samples)
                results.append({'connections': connections, 'size_mib': 128, 'seconds': samples, 'median_seconds': round(median, 3), 'median_mib_per_second': round(128 / median, 2)})
                print(json.dumps(results[-1]), flush=True)
        finally:
            app.stop()
finally:
    server.shutdown()
print(json.dumps({'speedup': round(results[0]['median_seconds'] / results[1]['median_seconds'], 2), 'conditions': 'loopback HTTP; 64 KiB writes with 2 ms delay per connection; 128 MiB; 3 runs each; includes merge and SHA-256', 'binary': str(Path(sys.argv[1] if len(sys.argv) > 1 else 'target/debug/flowdisk').resolve())}), flush=True)
