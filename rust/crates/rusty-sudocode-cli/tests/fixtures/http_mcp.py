"""Real HTTP MCP fixture with authorization and a causal call receipt."""

import json
import pathlib
import sys
import time
from http.server import BaseHTTPRequestHandler

from loopback_http import LoopbackHTTPServer

proof = pathlib.Path(sys.argv[1])
token = sys.argv[2]
call_delay = float(sys.argv[3]) if len(sys.argv) > 3 else 0


class MCP(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        if self.headers.get("Authorization") != "Bearer " + token:
            self.send_response(401)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        with proof.open("a", encoding="utf-8") as file:
            file.write(json.dumps(request) + "\n")
        method = request.get("method")
        if "id" not in request:
            self.send_response(202)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        if method == "initialize":
            value = {"protocolVersion": request["params"]["protocolVersion"], "capabilities": {"tools": {}}, "serverInfo": {"name": "remote-parity", "version": "1"}}
        elif method == "tools/list":
            value = {"tools": [{"name": "echo", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}}]}
        elif method == "tools/call":
            time.sleep(call_delay)
            value = {"content": [{"type": "text", "text": "echo:" + request["params"]["arguments"]["text"]}]}
        else:
            value = {}
        body = json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": value}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            # A deadline/cancellation is allowed to close an in-flight call.
            pass


server = LoopbackHTTPServer(("127.0.0.1", 0), MCP)
print(f"http://127.0.0.1:{server.server_address[1]}/mcp", flush=True)
server.serve_forever()
