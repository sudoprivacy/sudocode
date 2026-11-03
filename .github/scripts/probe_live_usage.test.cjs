const assert = require('node:assert/strict');
const { execFile } = require('node:child_process');
const fs = require('node:fs');
const http = require('node:http');
const os = require('node:os');
const path = require('node:path');
const { promisify } = require('node:util');
const test = require('node:test');

function removeFixture(directory) {
  const resolved = fs.realpathSync(directory);
  assert.equal(path.dirname(resolved).toLowerCase(), fs.realpathSync(os.tmpdir()).toLowerCase());
  assert.ok(path.basename(resolved).startsWith('scode-provider-probe-'));
  assert.equal(fs.lstatSync(directory).isSymbolicLink(), false);
  fs.rmSync(resolved, { recursive: true });
}

test('the real probe reports catalog and route failures without exposing credentials', async () => {
  const credential = 'fixture-provider-secret-12345678';
  const azureCredential = `${credential}-18`;
  const calls = [];
  const server = http.createServer(async (request, response) => {
    calls.push(`${request.method} ${request.url}`);
    if (request.url === '/v1/models') {
      assert.equal(request.headers.authorization, `Bearer ${credential}`);
      response.setHeader('content-type', 'application/json');
      response.end(JSON.stringify({ data: [{ id: 'fixture-model' }, { id: 'fixture-azure' }] }));
      return;
    }
    let body = '';
    for await (const chunk of request) body += chunk;
    const model = JSON.parse(body).model;
    assert.ok(['fixture-model', 'fixture-azure'].includes(model));
    const selectedCredential = model === 'fixture-azure' ? azureCredential : credential;
    assert.equal(request.headers.authorization, `Bearer ${selectedCredential}`);
    if (request.url === '/v1/messages' || model === 'fixture-azure') {
      response.writeHead(400, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ error: { type: 'invalid_request_error', code: 'model_not_found',
        message: `No route for credential ${selectedCredential}` } }));
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
          NATIVE_PROXY_AUTH_TOKEN: '', AZURE_PROXY_AUTH_TOKEN: '' },
        timeout: 15000, windowsHide: true,
      });
    const serialized = fs.readFileSync(output, 'utf8');
    assert.ok(!serialized.includes(credential));
    assert.ok(!stdout.includes(credential));
    const report = JSON.parse(serialized);
    assert.deepEqual(report[0], { route: 'models', profile: 'default', status: 200,
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
          SCODE_AZURE_MODEL: 'fixture-azure', NATIVE_PROXY_AUTH_TOKEN: '',
          AZURE_PROXY_AUTH_TOKEN: azureCredential },
        timeout: 15000, windowsHide: true,
      });
    const azureSerialized = fs.readFileSync(azureOutput, 'utf8');
    assert.ok(!azureSerialized.includes(credential));
    assert.ok(!azureSerialized.includes(azureCredential));
    const azureReport = JSON.parse(azureSerialized);
    assert.ok(azureReport[0].models.includes('fixture-azure'));
    const azureRoutes = azureReport.filter(item => item.model === 'fixture-azure');
    assert.equal(azureRoutes.length, 2);
    assert.ok(azureRoutes.every(item => item.profile === 'azure'
      && item.providerError.message === 'No route for credential [REDACTED]'));
    assert.ok(azureRoutes.every(item => item.status === 400 && item.providerError.code === 'model_not_found'),
      'catalog presence must never be reported as a working provider route');
  } finally {
    server.closeAllConnections();
    await new Promise(resolve => server.close(resolve));
    removeFixture(directory);
  }
});

test('the same model is probed independently through each configured auth profile', async () => {
  const defaultCredential = 'fixture-shared-provider-12345678';
  const nativeCredential = `${defaultCredential}-7`;
  const azureCredential = `${defaultCredential}-18`;
  const calls = [];
  const server = http.createServer(async (request, response) => {
    const credential = request.headers.authorization?.slice('Bearer '.length);
    calls.push({ url: request.url, credential });
    if (request.url === '/v1/models') {
      assert.equal(credential, defaultCredential);
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ data: [{ id: 'shared-model' }] }));
      return;
    }
    let body = '';
    for await (const chunk of request) body += chunk;
    assert.equal(JSON.parse(body).model, 'shared-model');
    assert.ok([defaultCredential, nativeCredential, azureCredential].includes(credential));
    if (credential === defaultCredential) {
      response.writeHead(400, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ error: { code: 'model_not_found', message: defaultCredential } }));
    } else {
      response.writeHead(200, { 'content-type': 'text/event-stream' });
      response.end('data: {"usage":{"prompt_tokens":3,"completion_tokens":1}}\n\ndata: [DONE]\n\n');
    }
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'scode-provider-probe-'));
  const env = { ...process.env, PROXY_AUTH_TOKEN: defaultCredential,
    NATIVE_PROXY_AUTH_TOKEN: nativeCredential, AZURE_PROXY_AUTH_TOKEN: azureCredential,
    PROXY_BASE_URL: `http://127.0.0.1:${server.address().port}/v1`,
    SCODE_LIVE_MODEL: 'shared-model', SCODE_NATIVE_MODEL: 'shared-model', SCODE_AZURE_MODEL: 'shared-model' };
  try {
    const output = path.join(directory, 'evidence.json');
    await promisify(execFile)(process.execPath,
      [path.join(__dirname, 'probe_live_usage.cjs'), output], { env, timeout: 15000, windowsHide: true });
    const serialized = fs.readFileSync(output, 'utf8');
    assert.ok([defaultCredential, nativeCredential, azureCredential].every(key => !serialized.includes(key)));
    const report = JSON.parse(serialized);
    assert.equal(report.length, 7);
    for (const profile of ['default', 'native', 'azure']) {
      const routes = report.filter(item => item.model === 'shared-model' && item.profile === profile);
      assert.equal(routes.length, 2);
      assert.ok(routes.every(item => item.status === (profile === 'default' ? 400 : 200)));
      if (profile !== 'default') assert.ok(routes.every(item => item.terminalSeen));
    }
    const before = calls.length;
    await assert.rejects(promisify(execFile)(process.execPath,
      [path.join(__dirname, 'probe_live_usage.cjs'), path.join(directory, 'missing.json')], {
        env: { ...env, AZURE_PROXY_AUTH_TOKEN: '' }, timeout: 15000, windowsHide: true,
      }), error => error.stderr.includes('AZURE_PROXY_AUTH_TOKEN is required')
        && !error.stderr.includes(azureCredential));
    assert.equal(calls.length, before, 'missing Azure auth must fail before making any provider request');
  } finally {
    server.closeAllConnections();
    await new Promise(resolve => server.close(resolve));
    removeFixture(directory);
  }
});

test('the workflow gives the Azure CLI profile the same scoped credential as its probe', async () => {
  const workflow = fs.readFileSync(path.join(__dirname, '../workflows/rust-ci.yml'), 'utf8');
  const blocks = [...workflow.matchAll(/node - <<'NODE'\r?\n([\s\S]*?)\r?\n[ \t]+NODE/g)];
  const generators = blocks.filter(match => match[1].includes("'sudorouter-azure'"));
  assert.equal(generators.length, 1);
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'scode-provider-probe-'));
  const env = { ...process.env, SUDO_CODE_CONFIG_HOME: directory,
    PROXY_BASE_URL: 'https://fixture-provider.invalid/v1',
    RUNNER_TEMP: directory, GITHUB_ENV: path.join(directory, 'github-env'),
    PROXY_AUTH_TOKEN: 'fixture-default-provider-12345678',
    AZURE_PROXY_AUTH_TOKEN: 'fixture-scoped-azure-provider-87654321', SCODE_AZURE_CHANNEL: '18' };
  try {
    await promisify(execFile)(process.execPath, ['-e', generators[0][1]],
      { env, timeout: 15000, windowsHide: true });
    const config = JSON.parse(fs.readFileSync(path.join(directory, 'sudocode.json'), 'utf8'));
    assert.equal(config.auth_modes.proxy.sudorouter.apiKey, env.PROXY_AUTH_TOKEN);
    assert.equal(config.auth_modes.proxy['sudorouter-azure'].apiKey, env.AZURE_PROXY_AUTH_TOKEN);
    assert.equal(config.auth_modes.proxy['sudorouter-azure'].baseUrl, config.auth_modes.proxy.sudorouter.baseUrl);
    assert.equal(config.auth_modes.proxy.sudorouter.baseUrl, env.PROXY_BASE_URL);
    const exported = fs.readFileSync(env.GITHUB_ENV, 'utf8');
    assert.ok(exported.includes(`SUDO_CODE_CONFIG_HOME=${directory}\n`));
    assert.ok(exported.includes(`SCODE_LOG_PATH=${path.join(directory, 'scode-live-model-trace.jsonl')}\n`));
    assert.ok(!exported.includes(env.PROXY_AUTH_TOKEN));
    assert.ok(!exported.includes(env.AZURE_PROXY_AUTH_TOKEN));
    await assert.rejects(promisify(execFile)(process.execPath, ['-e', generators[0][1]], {
      env: { ...env, AZURE_PROXY_AUTH_TOKEN: '' }, timeout: 15000, windowsHide: true,
    }), error => error.stderr.includes('AZURE_PROXY_AUTH_TOKEN is required')
      && !error.stderr.includes(env.AZURE_PROXY_AUTH_TOKEN));
  } finally {
    removeFixture(directory);
  }
});

test('provider preflight fails clearly on quota, auth and service errors and requires a completed route', async () => {
  const credential = 'fixture-preflight-secret-12345678';
  let fixture;
  const server = http.createServer((request, response) => {
    if (request.url === '/v1/models') {
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ data: [{ id: 'fixture-model' }] }));
      return;
    }
    if (fixture.streamFailure) {
      response.writeHead(200, { 'content-type': 'text/event-stream' });
      response.end(`data: ${JSON.stringify({type: 'error', error: {code: fixture.code, message: credential}})}\n\n`);
      return;
    }
    if ((fixture.ready && request.url === '/v1/chat/completions') || fixture.incomplete) {
      response.writeHead(200, { 'content-type': 'text/event-stream' });
      response.end('data: {"usage":{"prompt_tokens":3}}\n\n' + (fixture.ready ? 'data: [DONE]\n\n' : ''));
      return;
    }
    response.writeHead(fixture.status, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ error: { code: fixture.code, message: credential } }));
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'scode-provider-probe-'));
  try {
    for (fixture of [
      { category: 'quota', status: 403, code: 'pre_consume_token_quota_failed' },
      { category: 'authentication', status: 401, code: 'invalid_api_key' },
      { category: 'service', status: 503, code: 'unavailable' },
      { category: 'quota', streamFailure: true, code: 'insufficient_quota' },
      { category: 'protocol', incomplete: true },
      { category: 'ready', ready: true, status: 400, code: 'model_not_found' },
    ]) {
      const output = path.join(directory, `${fixture.category}.json`);
      let result;
      try {
        result = await promisify(execFile)(process.execPath,
          [path.join(__dirname, 'probe_live_usage.cjs'), output, '--require-ready'], {
            env: { ...process.env, PROXY_AUTH_TOKEN: credential,
              PROXY_BASE_URL: `http://127.0.0.1:${server.address().port}/v1`,
              SCODE_LIVE_MODEL: 'fixture-model', SCODE_NATIVE_MODEL: '', SCODE_AZURE_MODEL: '' },
            timeout: 15000, windowsHide: true,
          });
        assert.equal(fixture.category, 'ready');
      } catch (error) {
        result = error;
        assert.equal(error.code, 1);
        assert.ok(error.stderr.includes(`Live provider preflight (${fixture.category})`), error.stderr);
        assert.ok(error.stderr.includes('model acceptance has not passed'));
      }
      const evidence = fs.readFileSync(output, 'utf8');
      assert.ok(!evidence.includes(credential));
      assert.ok(!result.stderr.includes(credential));
      assert.ok(!result.stdout.includes(credential));
      if (!fixture.ready && !fixture.incomplete) {
        assert.ok(JSON.parse(evidence).filter(item => item.model)
          .every(item => item.failureCategory === fixture.category));
      }
    }
  } finally {
    server.closeAllConnections();
    await new Promise(resolve => server.close(resolve));
    removeFixture(directory);
  }
});

test('trace evidence identifies quota failures without labelling a successful retry as failed', async () => {
  const credential = 'fixture-trace-secret-12345678';
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'scode-provider-probe-'));
  try {
    const input = path.join(directory, 'trace.jsonl');
    const output = path.join(directory, 'evidence.json');
    fs.writeFileSync(input, ['failed', 'recovered'].flatMap(request_id => [
      { event: 'request_debug', attributes: { request_id, url: 'https://example.invalid/v1/messages', body: { model: 'fixture' } } },
      { event: 'response_debug', attributes: { request_id, status: request_id === 'failed' ? 403 : 200 } },
      { event: 'request_failed', attributes: { request_id, error: `pre_consume_token_quota_failed ${credential}` } },
    ]).map(event => JSON.stringify(event)).join('\n'));
    const result = await promisify(execFile)(process.execPath,
      [path.join(__dirname, 'summarize_live_model_trace.cjs'), input, output], {
        env: { ...process.env, PROXY_AUTH_TOKEN: credential }, timeout: 15000, windowsHide: true,
      });
    assert.equal(result.stderr.match(/Live provider failure \(quota\)/g)?.length, 1);
    const serialized = fs.readFileSync(output, 'utf8');
    assert.ok(!serialized.includes(credential));
    assert.ok(!result.stderr.includes(credential));
    const evidence = JSON.parse(serialized);
    assert.equal(evidence.requests[0].providerFailure, 'quota');
    assert.equal(evidence.requests[1].providerFailure, undefined);
  } finally {
    removeFixture(directory);
  }
});
