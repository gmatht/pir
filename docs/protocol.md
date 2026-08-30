# PIR <-> Node.js Shim Communication Protocol

This document defines the communication protocol between the PIR core (Rust) and the Node.js shim used to load legacy pi extensions.

## Transport
- Transport: Standard I/O (stdin/stdout).
- Format: Line-delimited JSON.
- Encoding: UTF-8.

## Message Format
All messages are JSON objects with the following fields:
- `id`: A unique identifier for the request. Responses must use the same `id`.
- `method`: The name of the function to call (for requests).
- `params`: An object containing arguments for the method.
- `result`: The return value of the method (for successful responses).
- `error`: An object containing error details (for failed responses).

## System Methods (PIR -> Shim)

### `load_extension`
Loads a pi extension from the specified path.
- Params: `{ "path": "string" }`
- Result: `{ "status": "loaded" | "error", "extensionId": "string" }`

### `call_extension`
Calls a method on a loaded extension.
- Params: `{ "extensionId": "string", "method": "string", "args": "any[]" }`
- Result: `{ "value": "any" }`

### `unload_extension`
Unloads an extension.
- Params: `{ "extensionId": "string" }`
- Result: `{ "status": "unloaded" }`

## System Methods (Shim -> PIR)

### `log`
Sends a log message from the Node.js environment to PIR.
- Params: `{ "level": "info" | "warn" | "error", "message": "string" }`
- Result: `{ "status": "ok" }`

### `request_feature`
Requests a PIR-side capability (e.g., filesystem access, terminal control).
- Params: `{ "feature": "string", "params": "any" }`
- Result: `{ "value": "any" }`
