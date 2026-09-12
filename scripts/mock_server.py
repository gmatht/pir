#!/usr/bin/env python3
"""Mock OpenAI-compatible server for pir/pi request-body parity tests.

Records every request (path, auth header, full JSON body) to a JSONL log and
answers POSTs with scripted SSE. The scenario is driven by `MOCK:` directive
lines in the last user message (mirroring pir's `FAKE:` test model), so neither
agent needs code changes to be scripted:

    MOCK: text            -- a few streamed text chunks, then end
    MOCK: tool <cmd>      -- streamed ack text + one bash tool call (<cmd>)
    MOCK: think <words>   -- OpenAI `delta.reasoning` chunks, then end
    MOCK: error <code>    -- HTTP error status (e.g. 429, 500)

Usage: mock_server.py <port> <logfile>
Stdlib only. Single-threaded accept + threaded handlers via ThreadingHTTPServer.
"""
import json
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8799
LOG = sys.argv[2] if len(sys.argv) > 2 else "/tmp/pir-mock-log.jsonl"


def log_record(rec):
    with open(LOG, "a") as f:
        f.write(json.dumps(rec) + "\n")


def user_text(body):
    try:
        msgs = body.get("messages", [])
        for m in reversed(msgs):
            if m.get("role") == "user":
                c = m.get("content", "")
                if isinstance(c, str) and c.strip():
                    return c
                if isinstance(c, list):
                    texts = [b.get("text", "") for b in c
                             if isinstance(b, dict) and b.get("type") == "text"]
                    joined = " ".join(t for t in texts if t).strip()
                    if joined:
                        return joined
    except Exception:
        pass
    return ""


def directives(text):
    out = []
    for line in text.splitlines():
        line = line.strip()
        if line.startswith("MOCK:"):
            out.append(line[len("MOCK:"):].strip())
    return out


def sse_chunk(payload):
    return ("data: " + json.dumps(payload) + "\n\n").encode()


def text_chunks(words, delay=0.05):
    for w in words.split():
        yield sse_chunk({"choices": [{"delta": {"content": w + " "}, "index": 0}]})
        time.sleep(delay)


def history_has_tools(body):
    """True once any tool call/result is already in the conversation: the
    follow-up round must conclude with text, never re-emit tools (or the
    turn would loop forever)."""
    try:
        for m in body.get("messages", []):
            if m.get("role") == "tool":
                return True
            for tc in (m.get("tool_calls") or []):
                return True
            content = m.get("content")
            if isinstance(content, list):
                for b in content:
                    if isinstance(b, dict) and b.get("type") in ("tool_use", "tool_result"):
                        return True
    except Exception:
        pass
    return False


def run(directives_list, conclude_only):
    ntools = 0
    if conclude_only:
        yield from text_chunks("mock concluding after tools")
        yield sse_chunk({"usage": {"prompt_tokens": 10, "completion_tokens": 5}})
        yield sse_chunk({"choices": [{"delta": {}, "index": 0, "finish_reason": "stop"}]})
        yield b"data: [DONE]\n\n"
        return
    for d in directives_list:
        verb, _, rest = d.partition(" ")
        verb = verb.lower()
        if verb == "text":
            text = rest.strip() or "mock reply ok"
            yield from text_chunks(text)
        elif verb == "tool":
            cmd = rest.strip() or "echo wire-ok"
            yield from text_chunks("mock: running tool")
            yield sse_chunk({
                "choices": [{
                    "delta": {"tool_calls": [{
                        "index": ntools,
                        "id": f"mock-call-{ntools}",
                        "function": {
                            "name": "bash",
                            "arguments": json.dumps({"command": cmd}),
                        },
                    }]},
                    "index": 0,
                }],
            })
            ntools += 1
        elif verb == "think":
            words = (rest.strip() or "pondering").split()
            for w in words:
                yield sse_chunk({"choices": [{"delta": {"reasoning": w + " "}, "index": 0}]})
                time.sleep(0.05)
        # unknown verbs: ignored (a mock must never break a session)
    if ntools == 0 and not any(d.split(" ")[0].lower() in ("text", "think") for d in directives_list):
        yield from text_chunks("mock default ok")
    # usage + terminator (pir reads prompt/completion tokens off this object;
    # pi requires an explicit finish_reason event before [DONE]).
    yield sse_chunk({"usage": {"prompt_tokens": 10, "completion_tokens": 5}})
    yield sse_chunk({"choices": [{"delta": {}, "index": 0, "finish_reason": "stop"}]})
    yield b"data: [DONE]\n\n"


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def do_GET(self):
        body = b'{"ok": true}\n'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0) or 0)
        raw = self.rfile.read(length) if length else b"{}"
        try:
            body = json.loads(raw.decode("utf-8", "replace"))
        except Exception:
            body = {}
        log_record({
            "ts": time.time(),
            "path": self.path,
            "auth_present": bool(self.headers.get("Authorization") or self.headers.get("x-api-key")),
            "body": body,
        })
        text = user_text(body)
        dirs = directives(text)
        err = next((d for d in dirs if d.split(" ")[0].lower() == "error"), None)
        if err:
            try:
                code = int(err.split(" ")[1])
            except Exception:
                code = 500
            body_out = b'{"error": "mock induced error"}\n'
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body_out)))
            self.end_headers()
            self.wfile.write(body_out)
            return
        chunks = list(run(dirs, history_has_tools(body)))
        blob = b"".join(chunks)
        # chunked streaming would be ideal; content-length framing is simpler
        # and both pir and pi parse `data:` lines either way.
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(blob)))
        self.end_headers()
        self.wfile.write(blob)


if __name__ == "__main__":
    open(LOG, "w").write("")
    srv = ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
    print(f"mock server on 127.0.0.1:{PORT} logging to {LOG}", flush=True)
    srv.serve_forever()
