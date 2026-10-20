"""Real HTTP MCP fixture with authorization and a causal call receipt."""

import json
import pathlib
import socketserver
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

proof = pathlib.Path(sys.argv[1])
token = sys.argv[2]


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
            value = {"content": [{"type": "text", "text": "echo:" + request["params"]["arguments"]["text"]}]}
        else:
            value = {}
        body = json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": value}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class LocalHTTPServer(ThreadingHTTPServer):
    def server_bind(self):
        # Loopback fixtures need no reverse DNS; getfqdn can block on loaded
        # macOS runners before the readiness line is written.
        socketserver.TCPServer.server_bind(self)
        self.server_name = "127.0.0.1"
        self.server_port = self.server_address[1]


server = LocalHTTPServer(("127.0.0.1", 0), MCP)
print(f"http://127.0.0.1:{server.server_address[1]}/mcp", flush=True)
server.serve_forever()
