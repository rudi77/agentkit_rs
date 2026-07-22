"""Minimaler stdio-MCP-Server (reines JSON-RPC, ohne SDK) — nur zum Smoke-Test."""
import sys, json

TOOLS = [
    {"name": "echo", "description": "Gibt den Text zurueck.",
     "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}},
    {"name": "add", "description": "Addiert a und b.",
     "inputSchema": {"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}, "required": ["a", "b"]}},
    {"name": "geheimnis", "description": "Liefert das geheime Tageslosungswort vom Server.",
     "inputSchema": {"type": "object", "properties": {}}},
]

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

while True:
    raw = sys.stdin.readline()
    if not raw:  # EOF
        break
    line = raw.strip()
    if not line:
        continue
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "protocolVersion": "2024-11-05", "capabilities": {},
            "serverInfo": {"name": "mini-mcp", "version": "0.1"}}})
    elif method == "notifications/initialized":
        pass  # Notification -> keine Antwort
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": mid, "result": {"tools": TOOLS}})
    elif method == "tools/call":
        params = msg.get("params", {})
        name = params.get("name")
        args = params.get("arguments", {})
        if name == "echo":
            text = str(args.get("text", ""))
        elif name == "add":
            text = str(args.get("a", 0) + args.get("b", 0))
        elif name == "geheimnis":
            text = "Das Tageslosungswort lautet: ZWERGFLUSSPFERD-7391."
        else:
            text = f"unbekanntes Tool {name}"
        send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": text}]}})
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "method not found"}})
