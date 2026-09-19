#!/usr/bin/env python3
"""Range-capable static server, for testing download resume.

Python's stock http.server ignores Range, so a resumed download would silently
restart from zero there. This one honours it and logs every request's Range
header, which is what makes "did it actually resume?" answerable.
"""
import os
import re
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = sys.argv[1]
PORT = int(sys.argv[2])
# Seconds to sleep per 64 KiB chunk, so a download can be interrupted mid-flight.
THROTTLE = float(sys.argv[3]) if len(sys.argv) > 3 else 0.0


class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        pass

    def do_HEAD(self):
        self.serve(body=False)

    def do_GET(self):
        self.serve(body=True)

    def serve(self, body):
        path = os.path.join(ROOT, os.path.basename(self.path))
        if not os.path.isfile(path):
            self.send_error(404)
            return
        size = os.path.getsize(path)
        start, end = 0, size - 1
        rng = self.headers.get("Range")
        status = 200
        if rng:
            m = re.match(r"bytes=(\d+)-(\d*)$", rng.strip())
            if m:
                start = int(m.group(1))
                if m.group(2):
                    end = int(m.group(2))
                status = 206
        length = max(0, end - start + 1)
        print(
            f"REQ {os.path.basename(self.path)} range={rng or '-'} "
            f"status={status} bytes={length}",
            flush=True,
        )
        self.send_response(status)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(length))
        self.send_header("Accept-Ranges", "bytes")
        if status == 206:
            self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
        self.end_headers()
        if not body:
            return
        with open(path, "rb") as handle:
            handle.seek(start)
            remaining = length
            while remaining > 0:
                chunk = handle.read(min(64 * 1024, remaining))
                if not chunk:
                    break
                self.wfile.write(chunk)
                remaining -= len(chunk)
                if THROTTLE:
                    time.sleep(THROTTLE)


ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
