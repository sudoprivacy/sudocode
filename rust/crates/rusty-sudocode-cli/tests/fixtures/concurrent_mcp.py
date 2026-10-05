"""Real MCP server whose file gates expose execution ordering to PTY tests."""
import concurrent.futures
import http.server
import json
from pathlib import Path
import sys
import threading
import time

lock = threading.Lock()
labels = {}

def process(request):
    method, params = request.get('method'), request.get('params', {})
    if method == 'initialize':
        return {'protocolVersion': '2025-03-26', 'capabilities': {'tools': {}}, 'serverInfo': {'name': 'parallel-fixture', 'version': '1'}}
    if method == 'tools/list':
        schema = {'type': 'object', 'properties': {'label': {'type': 'string'}}, 'required': ['label']}
        return {'tools': [{'name': name, 'description': 'Wait for the fixture release gate, return the label', 'inputSchema': schema, 'annotations': {'readOnlyHint': safe}} for name, safe in [('read', True), ('write', False)]]}
    if method == 'tools/call':
        label = params.get('arguments', {}).get('label', '')
        assert label.isalnum(), 'fixture labels must be alphanumeric'
        labels[request['id']] = label
        Path(label + '.started').touch()
        until = time.monotonic() + 40
        while params['name'] == 'read' and not Path(label + '.release').exists():
            if time.monotonic() > until:
                raise TimeoutError('fixture was not released')
            time.sleep(.01)
        Path(label + '.finished').touch()
        return {'content': [{'type': 'text', 'text': 'MCP_RESULT_' + label}], 'isError': label == 'error'}
    return {}

def response(request):
    if request.get('method') == 'notifications/cancelled':
        label = labels.get(request['params']['requestId'])
        if label:
            Path(label + '.cancelled').touch()
    if request.get('id') is None:
        return None
    try:
        return {'jsonrpc': '2.0', 'id': request['id'], 'result': process(request)}
    except Exception as error:
        return {'jsonrpc': '2.0', 'id': request['id'], 'error': {'code': -32603, 'message': str(error)}}

def stdio(request):
    result = response(request)
    if result is not None:
        with lock:
            print(json.dumps(result), flush=True)

class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        if request.get('method') != 'initialize':
            assert self.headers.get('Mcp-Session-Id') == 'parallel-session'
        result = response(request)
        body = json.dumps(result).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        if request.get('method') == 'initialize':
            self.send_header('Mcp-Session-Id', 'parallel-session')
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *args):
        pass

if len(sys.argv) > 1:
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    Path('http-port').write_text(str(server.server_port))
    server.serve_forever()
else:
    with concurrent.futures.ThreadPoolExecutor(max_workers=12) as pool:
        for line in sys.stdin:
            pool.submit(stdio, json.loads(line))
