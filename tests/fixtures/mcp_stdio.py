"""A tiny stdio MCP server for tests/mcp_gateway.rs: one `echo` tool that greets with $GREETING."""
import json
import os
import sys


def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


print("mock stdio server starting", file=sys.stderr, flush=True)
# Some servers log to stdout; the gateway must skip lines that are not JSON-RPC.
print("not json", flush=True)
for line in sys.stdin:
    msg = json.loads(line)
    method, id_ = msg.get("method"), msg.get("id")
    if id_ is None:
        continue
    if method == "initialize":
        result = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "py", "version": "1"}}
    elif method == "tools/list":
        schema = {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}
        result = {"tools": [{"name": "echo", "description": "Echo text back with a greeting", "inputSchema": schema}]}
    elif method == "tools/call":
        text = msg["params"]["arguments"].get("text", "")
        result = {"content": [{"type": "text", "text": os.environ.get("GREETING", "") + " " + text}]}
    else:
        send({"jsonrpc": "2.0", "id": id_, "error": {"code": -32601, "message": "method not found"}})
        continue
    send({"jsonrpc": "2.0", "id": id_, "result": result})
