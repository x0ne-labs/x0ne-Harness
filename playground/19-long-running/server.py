#!/usr/bin/env python3
"""A tiny HTTP server for exercise 19. Serves "pong" on /ping and writes
`stopped.txt` when it receives SIGTERM — which is how a clean stop is graded.
Usage: python3 server.py <port>"""
import http.server, signal, sys, pathlib

port = int(sys.argv[1]) if len(sys.argv) > 1 else 8765
here = pathlib.Path(__file__).resolve().parent

class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b"pong\n" if self.path == "/ping" else b"exercise 19\n"
        self.send_response(200); self.send_header("Content-Length", str(len(body))); self.end_headers()
        self.wfile.write(body)
    def log_message(self, *a): print("served", self.path, flush=True)

srv = http.server.HTTPServer(("127.0.0.1", port), H)
def stop(*_):
    (here / "stopped.txt").write_text("stopped cleanly\n")
    print("stopping", flush=True)
    raise SystemExit(0)
signal.signal(signal.SIGTERM, stop)
signal.signal(signal.SIGINT, stop)
(here / "started.txt").write_text(f"{port}\n")
print(f"listening on 127.0.0.1:{port}", flush=True)
srv.serve_forever()
