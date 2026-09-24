#!/usr/bin/env python3
"""A minimal MCP stdio server, used by pir's tests and as a worked example.

Implements just enough of the protocol to be a real server: `initialize`,
`notifications/initialized`, `tools/list`, `tools/call`. Speaks newline-delimited
JSON-RPC 2.0 on stdin/stdout, exactly as the spec requires.

Tools:
  echo        — returns its `text` argument
  add         — returns the sum of `a` and `b`
  fail        — returns a JSON-RPC error, to exercise the error path
  emit_image  — returns a non-text content part, to prove it is named not dumped
  slow        — sleeps, to exercise timeouts

Run it directly to poke at it by hand:

    echo '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | ./mcp_test_server.py
"""

import json
import sys
import time

TOOLS = [
    {
        "name": "echo",
        "description": "Echo the given text back",
        "inputSchema": {
            "type": "object",
            "properties": {"text": {"type": "string", "description": "text to echo"}},
            "required": ["text"],
        },
    },
    {
        "name": "add",
        "description": "Add two numbers",
        "inputSchema": {
            "type": "object",
            "properties": {
                "a": {"type": "number"},
                "b": {"type": "number"},
            },
            "required": ["a", "b"],
        },
    },
    {
        "name": "fail",
        "description": "Always fails, for testing error handling",
        "inputSchema": {"type": "object", "properties": {}},
    },
    {
        "name": "emit_image",
        "description": "Returns a non-text content part",
        "inputSchema": {"type": "object", "properties": {}},
    },
    {
        "name": "slow",
        "description": "Sleeps before answering, for testing timeouts",
        "inputSchema": {
            "type": "object",
            "properties": {"seconds": {"type": "number"}},
        },
    },
]


def respond(msg_id, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": msg_id, "result": result}) + "\n")
    sys.stdout.flush()


def respond_error(msg_id, code, message):
    sys.stdout.write(
        json.dumps({"jsonrpc": "2.0", "id": msg_id, "error": {"code": code, "message": message}})
        + "\n"
    )
    sys.stdout.flush()


def handle(msg):
    method = msg.get("method")
    msg_id = msg.get("id")
    params = msg.get("params") or {}

    # A notification has no id: never reply.
    if msg_id is None:
        return

    if method == "initialize":
        respond(msg_id, {
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "pir-test-server", "version": "1.0.0"},
        })
    elif method == "tools/list":
        respond(msg_id, {"tools": TOOLS})
    elif method == "tools/call":
        name = params.get("name")
        args = params.get("arguments") or {}
        if name == "echo":
            respond(msg_id, {"content": [{"type": "text", "text": args.get("text", "")}]})
        elif name == "add":
            total = args.get("a", 0) + args.get("b", 0)
            respond(msg_id, {"content": [{"type": "text", "text": str(total)}]})
        elif name == "fail":
            respond_error(msg_id, -32000, "this tool always fails")
        elif name == "emit_image":
            respond(msg_id, {"content": [
                {"type": "text", "text": "here is an image"},
                {"type": "image", "mimeType": "image/png", "data": "iVBORw0KGgoAAAANSUhEUg=="},
            ]})
        elif name == "slow":
            time.sleep(float(args.get("seconds", 1)))
            respond(msg_id, {"content": [{"type": "text", "text": "finally"}]})
        else:
            respond_error(msg_id, -32602, f"unknown tool '{name}'")
    else:
        respond_error(msg_id, -32601, f"unknown method '{method}'")


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        handle(msg)


if __name__ == "__main__":
    main()
