"""Terminate real TLS in front of the existing local protocol fixture."""

import http.client
import http.server
import json
from pathlib import Path
import ssl
import sys
from urllib.parse import urlsplit


def main():
    certificate, key, backend_url, ready_path = sys.argv[1:]
    backend = urlsplit(backend_url)
    assert backend.scheme == "http" and backend.hostname == "127.0.0.1"

    class Gateway(http.server.BaseHTTPRequestHandler):
        def forward(self):
            connection = http.client.HTTPConnection(backend.hostname, backend.port, timeout=30)
            excluded = {"host", "connection", "transfer-encoding", "content-length"}
            headers = {name: value for name, value in self.headers.items() if name.lower() not in excluded}
            body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            try:
                connection.request(self.command, self.path, body=body, headers=headers)
                response = connection.getresponse()
                content = response.read()
                self.send_response(response.status)
                for name, value in response.getheaders():
                    if name.lower() not in excluded:
                        self.send_header(name, value)
                self.send_header("Content-Length", str(len(content)))
                self.end_headers()
                self.wfile.write(content)
            finally:
                connection.close()

        do_GET = forward
        do_POST = forward

        def log_message(self, _format, *_args):
            pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Gateway)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(certificate, key)
    server.socket = context.wrap_socket(server.socket, server_side=True)
    # Publish only after binding and configuring TLS; no fixed port or startup sleep.
    Path(ready_path).write_text(json.dumps({"port": server.server_port}), encoding="utf-8")
    server.serve_forever()


if __name__ == "__main__":
    main()
