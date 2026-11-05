"""Scripted stand-in for the model in docs/assets/tui.gif.

A loopback OpenAI-compatible endpoint whose replies are written below:
no model runs and no key is sent. tui-demo.sh starts it on port 9931.

Usage: python3 tui-demo-model.py PORT LOG
"""
import http.server
import json
import sys
import time

DEMO = "/tmp/demo/acme-api"
PROMPTS = (
    "What does this project do? Also check the API key in .env",
    "Run the tests",
)


def act(action):
    return "<action>" + json.dumps(action) + "</action>"


# SCRIPT[p][k] answers prompt p after k earlier replies to it.
SCRIPT = (
    (
        "Reading the README first.\n" + act({"type": "ReadFile", "path": f"{DEMO}/README.md"}),
        "Now the key in .env.\n" + act({"type": "ReadFile", "path": f"{DEMO}/.env"}),
        "acme-api is a small payments API: `charge(amount_cents)` returns a receipt "
        "for a positive amount.\n\nReading .env was refused by the policy, so I did not see the key.",
    ),
    (
        act({"type": "RunCommand", "command": "python3", "args": ["-m", "unittest", "-v"], "cwd": DEMO}),
        "All 3 tests pass.",
    ),
)


def reply_for(messages):
    for i in range(len(messages) - 1, -1, -1):
        m = messages[i]
        text = m.get("content")
        if m.get("role") == "user" and isinstance(text, str) and text.strip() in PROMPTS:
            p = PROMPTS.index(text.strip())
            k = sum(1 for n in messages[i + 1:] if n.get("role") == "assistant")
            if k < len(SCRIPT[p]):
                return SCRIPT[p][k]
    return "(no scripted reply)"


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        self.send_json({"object": "list", "data": [{"id": "scripted-demo", "object": "model"}]})

    def do_POST(self):
        n = int(self.headers.get("content-length") or 0)
        body = json.loads(self.rfile.read(n) or b"{}")
        with open(LOG, "a") as f:
            f.write(json.dumps({"t": time.time(), "path": self.path,
                                "authorization": "authorization" in self.headers,
                                "body": body}) + "\n")
        text = reply_for(body.get("messages", []))
        usage = {"prompt_tokens": n // 4, "completion_tokens": len(text) // 4}
        time.sleep(0.6)
        if not body.get("stream"):
            self.send_json({"choices": [{"index": 0, "finish_reason": "stop",
                                         "message": {"role": "assistant", "content": text}}],
                            "usage": usage})
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Connection", "close")
        self.end_headers()
        for i in range(0, len(text), 16):
            self.sse({"choices": [{"index": 0, "delta": {"content": text[i:i + 16]}}]})
            time.sleep(0.02)
        self.sse({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]})
        self.sse({"choices": [], "usage": usage})
        self.wfile.write(b"data: [DONE]\n\n")

    def sse(self, obj):
        self.wfile.write(b"data: " + json.dumps(obj).encode() + b"\n\n")
        self.wfile.flush()

    def send_json(self, obj):
        b = json.dumps(obj).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def log_message(self, *args):
        pass


LOG = sys.argv[2]
http.server.ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
