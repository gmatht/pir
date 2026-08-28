import http.server
import json
import socketserver


class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        self.rfile.read(length)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        one = {"choices": [{"delta": {"content": "done"}}]}
        two = {"choices": [{"delta": {"content": ""}}],
               "usage": {"prompt_tokens": 5, "completion_tokens": 1}}
        self.wfile.write(("data: " + json.dumps(one) + "\n\n").encode())
        self.wfile.write(("data: " + json.dumps(two) + "\n\n").encode())
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()

    def log_message(self, *a):
        pass


socketserver.TCPServer(("127.0.0.1", 8765), H).serve_forever()
