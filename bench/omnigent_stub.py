"""Omnigent and proxy stub for amh tests and benchmarks: the subset of the HTTP API amh calls, held in memory.

Usage: `omnigent_stub.py PORT_FILE [--latency-ms N] [--fail-rate P] [--history-delay-s S]`.
Binds an ephemeral localhost port and writes it to `PORT_FILE`.
`GET /stub/stats` reports counters; `GET /stub/messages` lists every received message with its arrival time.
Any path outside `/v1/` answers 200 `{}`, so the same server stands in for the proxy.
"""

import argparse
import json
import random
import re
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse

AGENTS = [{"id": f"agent-{t}", "name": f"{t}-native-ui"} for t in ("claude", "codex", "cursor", "antigravity")]
SESSION_PATH = re.compile(r"^/v1/sessions/([^/]+)(/events|/items|/resources)?$")


class State:
    """All stub data; every access holds `lock`, since handlers run on many threads."""

    def __init__(self, args):
        self.args = args
        self.lock = threading.Lock()
        self.sessions = {}
        self.messages = []
        self.creating = 0
        self.max_creating = 0
        self.n_requests = 0


def session_view(sid, session):
    return {
        "id": sid,
        "status": "idle",
        "runner_online": True,
        "harness": session["harness"],
        "labels": {},
        "updated_at": session["created_at"],
        "pending_elicitations": [],
    }


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def answer(self, code, body):
        data = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def body(self):
        n = int(self.headers.get("Content-Length") or 0)
        return json.loads(self.rfile.read(n) or b"null")

    def do_GET(self):
        self.route("GET", None)

    def do_POST(self):
        self.route("POST", self.body())

    def do_PATCH(self):
        self.route("PATCH", self.body())

    def route(self, method, body):
        st: State = self.server.state
        path = urlparse(self.path).path
        with st.lock:
            st.n_requests += 1
        if path == "/stub/stats":
            with st.lock:
                with_msgs = sum(1 for s in st.sessions.values() if s["items"])
                return self.answer(200, {"sessions": len(st.sessions), "sessions_with_messages": with_msgs,
                                         "messages": len(st.messages), "max_create_in_flight": st.max_creating,
                                         "requests": st.n_requests})
        if path == "/stub/messages":
            with st.lock:
                return self.answer(200, list(st.messages))
        if not path.startswith("/v1/"):
            return self.answer(200, {})
        if random.random() < st.args.fail_rate:
            return self.answer(503, {"error": "stub failure"})
        is_create = method == "POST" and path == "/v1/sessions"
        if is_create:
            with st.lock:
                st.creating += 1
                st.max_creating = max(st.max_creating, st.creating)
        time.sleep(st.args.latency_ms / 1000)
        try:
            self.serve_v1(st, method, path, body)
        finally:
            if is_create:
                with st.lock:
                    st.creating -= 1

    def serve_v1(self, st, method, path, body):
        if path == "/v1/agents":
            return self.answer(200, {"data": AGENTS})
        if path == "/v1/hosts":
            return self.answer(200, {"hosts": [{"host_id": "host-stub", "status": "online"}]})
        if method == "POST" and path == "/v1/sessions":
            sid = uuid.uuid4().hex
            harness = next((a["name"] for a in AGENTS if a["id"] == body.get("agent_id")), "claude-native-ui")
            with st.lock:
                st.sessions[sid] = {"harness": harness, "created_at": time.time(), "items": []}
            return self.answer(200, {"id": sid})
        found = SESSION_PATH.match(path)
        with st.lock:
            session = st.sessions.get(found.group(1)) if found else None
            if session is None:
                return self.answer(404, {"error": "no such session"})
            sub = found.group(2)
            now = time.time()
            if sub == "/events" and method == "POST":
                if body.get("type") == "message":
                    text = body["data"]["content"][0]["text"]
                    visible_at = now + st.args.history_delay_s
                    session["items"].append({"visible_at": visible_at, "item": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}})
                    st.messages.append({"session": found.group(1), "recv_ns": time.time_ns(), "text": text})
                return self.answer(200, {"queued": True})
            if sub == "/items":
                items = [i["item"] for i in reversed(session["items"]) if i["visible_at"] <= now]
                return self.answer(200, {"data": items[:100]})
            if sub == "/resources":
                return self.answer(200, {"data": []})
            return self.answer(200, session_view(found.group(1), session))


class Server(ThreadingHTTPServer):
    daemon_threads = True
    request_queue_size = 1024


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("port_file")
    parser.add_argument("--latency-ms", type=float, default=5)
    parser.add_argument("--fail-rate", type=float, default=0)
    parser.add_argument("--history-delay-s", type=float, default=0)
    args = parser.parse_args()
    server = Server(("127.0.0.1", 0), Handler)
    server.state = State(args)
    with open(args.port_file, "w") as f:
        f.write(str(server.server_address[1]))
    server.serve_forever()


if __name__ == "__main__":
    main()
