const assert = require('node:assert/strict');
const { execFile } = require('node:child_process');
const fs = require('node:fs');
const http = require('node:http');
const os = require('node:os');
const path = require('node:path');
const { promisify } = require('node:util');
const test = require('node:test');

test('the real probe reports catalog and route failures without exposing credentials', async () => {
  const credential = 'fixture-provider-secret-12345678';
  const calls = [];
  const server = http.createServer(async (request, response) => {
    calls.push(`${request.method} ${request.url}`);
    assert.equal(request.headers.authorization, `Bearer ${credential}`);
    if (request.url === '/v1/models') {
      response.setHeader('content-type', 'application/json');
      response.end(JSON.stringify({ data: [{ id: 'fixture-model' }, { id: 'fixture-azure' }] }));
      return;
    }
    let body = '';
    for await (const chunk of request) body += chunk;
    const model = JSON.parse(body).model;
    assert.ok(['fixture-model', 'fixture-azure'].includes(model));
    if (request.url === '/v1/messages' || model === 'fixture-azure') {
      response.writeHead(400, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ error: { type: 'invalid_request_error', code: 'model_not_found',
        message: `No route for credential ${credential}` } }));
    } else {
      assert.equal(request.url, '/v1/chat/completions');
      response.writeHead(200, { 'content-type': 'text/event-stream' });
      response.end('data: {"usage":{"prompt_tokens":12,"completion_tokens":1,"total_tokens":13}}\n\ndata: [DONE]\n\n');
    }
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'scode-provider-probe-'));
  try {
    const output = path.join(directory, 'evidence.json');
    const { stdout } = await promisify(execFile)(process.execPath,
      [path.join(__dirname, 'probe_live_usage.cjs'), output], {
        env: { ...process.env, PROXY_AUTH_TOKEN: credential,
          PROXY_BASE_URL: `http://127.0.0.1:${server.address().port}/v1`,
          SCODE_LIVE_MODEL: 'fixture-model', SCODE_NATIVE_MODEL: '', SCODE_AZURE_MODEL: '',
          NATIVE_PROXY_AUTH_TOKEN: '' },
        timeout: 15000, windowsHide: true,
      });
    const serialized = fs.readFileSync(output, 'utf8');
    assert.ok(!serialized.includes(credential));
    assert.ok(!stdout.includes(credential));
    const report = JSON.parse(serialized);
    assert.deepEqual(report[0], { route: 'models', status: 200,
      models: ['fixture-azure', 'fixture-model'] });
    assert.equal(report[1].route, 'chat/completions');
    assert.equal(report[1].terminalSeen, true);
    assert.equal(report[1].usage[0].counts.total_tokens, 13);
    assert.equal(report[2].route, 'messages');
    assert.equal(report[2].status, 400);
    assert.equal(report[2].providerError.code, 'model_not_found');
    assert.equal(report[2].providerError.message, 'No route for credential [REDACTED]');
    assert.deepEqual(calls, ['GET /v1/models', 'POST /v1/chat/completions', 'POST /v1/messages']);
    const azureOutput = path.join(directory, 'azure-evidence.json');
    await promisify(execFile)(process.execPath,
      [path.join(__dirname, 'probe_live_usage.cjs'), azureOutput], {
        env: { ...process.env, PROXY_AUTH_TOKEN: credential,
          PROXY_BASE_URL: `http://127.0.0.1:${server.address().port}/v1`,
          SCODE_LIVE_MODEL: 'fixture-model', SCODE_NATIVE_MODEL: '',
          SCODE_AZURE_MODEL: 'fixture-azure', NATIVE_PROXY_AUTH_TOKEN: '' },
        timeout: 15000, windowsHide: true,
      });
    const azureSerialized = fs.readFileSync(azureOutput, 'utf8');
    assert.ok(!azureSerialized.includes(credential));
    const azureReport = JSON.parse(azureSerialized);
    assert.ok(azureReport[0].models.includes('fixture-azure'));
    const azureRoutes = azureReport.filter(item => item.model === 'fixture-azure');
    assert.equal(azureRoutes.length, 2);
    assert.ok(azureRoutes.every(item => item.status === 400 && item.providerError.code === 'model_not_found'),
      'catalog presence must never be reported as a working provider route');
  } finally {
    server.closeAllConnections();
    await new Promise(resolve => server.close(resolve));
    const resolved = fs.realpathSync(directory);
    assert.equal(path.dirname(resolved).toLowerCase(), fs.realpathSync(os.tmpdir()).toLowerCase());
    assert.ok(path.basename(resolved).startsWith('scode-provider-probe-'));
    assert.equal(fs.lstatSync(directory).isSymbolicLink(), false);
    fs.rmSync(resolved, { recursive: true });
  }
});
