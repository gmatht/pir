#!/usr/bin/env python
"""Mock SSE server for retry-freeze reproduction. (py2)

Answers 200 + text/event-stream, emits one empty text_delta (Anthropic shape:
on_text("") stops the thinking spinner), then hard-closes with an RST so the
client sees a mid-stream transport error and retries (emitted_text stays false
because the delta was empty). Keeps the client in the retry loop indefinitely.

Usage: mock_sse.py <port> [reset| eof| openai]
"""
from __future__ import print_function
import socket, struct, sys, threading, time

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8765
MODE = sys.argv[2] if len(sys.argv) > 2 else "reset"
DELAY = float(sys.argv[3]) if len(sys.argv) > 3 else 0.05

ANTHROPIC_EMPTY = (
    b"event: content_block_delta\r\n"
    b'data: {"type":"content_block_delta","delta":{"type":"text_delta","text":""}}\r\n\r\n'
)
OPENAI_EMPTY = (
    b'data: {"id":"x","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}\r\n\r\n'
)


def serve(conn, addr):
    try:
        conn.settimeout(2)
        try:
            conn.recv(65536)
        except Exception:
            pass
        conn.sendall(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")
        time.sleep(DELAY)
        conn.sendall(OPENAI_EMPTY if MODE == "openai" else ANTHROPIC_EMPTY)
        if MODE == "reset":
            conn.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
        conn.close()
        sys.stderr.write("[mock] served %s:%s mode=%s\n" % (addr[0], addr[1], MODE))
        sys.stderr.flush()
    except Exception as e:
        sys.stderr.write("[mock] err %s\n" % e)
        sys.stderr.flush()


def main():
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", PORT))
    s.listen(64)
    sys.stderr.write("[mock] listening on 127.0.0.1:%d mode=%s\n" % (PORT, MODE))
    sys.stderr.flush()
    while True:
        conn, addr = s.accept()
        threading.Thread(target=serve, args=(conn, addr)).start()


if __name__ == "__main__":
    main()
