"""One-shot MCP client for manual checks: python client.py <tool> '<json arguments>'."""
import json
import os
import subprocess
import sys
from pathlib import Path

exe = os.environ.get("RAPIDRAW_MCP_EXE") or str(Path(__file__).resolve().parent.parent / "server/target/debug/rapidraw-mcp.exe")
process = subprocess.Popen([exe], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, encoding="utf-8")


def send(message):
    process.stdin.write(json.dumps(message) + "\n")
    process.stdin.flush()


def receive(identifier):
    for line in process.stdout:
        reply = json.loads(line)
        if reply.get("id") == identifier:
            return reply


send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
    "protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "cli", "version": "1"}}})
receive(1)
send({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}})
send({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
    "name": sys.argv[1], "arguments": json.loads(sys.argv[2]) if len(sys.argv) > 2 else {}}})
result = receive(2)["result"]
import base64, os
saved = 0
for part in result["content"]:
    if part["type"] == "image":
        target = os.environ.get("SAVE_IMAGE")
        if target:
            Path(target).write_bytes(base64.b64decode(part["data"]))
            saved += 1
        print("[image %d bytes base64]" % len(part["data"]))
    else:
        print(part["text"])
print("ERROR" if result.get("isError") else "ok")
process.stdin.close()
process.wait(timeout=5)
