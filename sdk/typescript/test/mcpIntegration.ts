import * as assert from 'node:assert/strict';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';
const root = path.resolve(__dirname, '../..');
const { connectScode, sendScodeMessage, scodeNativeA2aAudit, forwardScodeToCodex } = require(path.join(root, 'dist/index.cjs')) as typeof import('../src/index');
const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'scode-acp-mcp-'));
const fixture = path.join(root, 'test/fixtures/scode-direct-a2a-agent.mjs');
const journal = (file: string) => fs.existsSync(file)
  ? fs.readFileSync(file, 'utf8').trim().split('\n').filter(Boolean).map(line => JSON.parse(line) as Record<string, unknown>) : [];

async function checkMcp(scenario: string): Promise<void> {
  const config = path.join(directory, `${scenario}.json`);
  const log = path.join(directory, `${scenario}-fixture.jsonl`);
  const auditLog = path.join(directory, `${scenario}-audit.jsonl`);
  fs.writeFileSync(config, JSON.stringify({ version: 1, scodeArgv: [process.execPath, fixture, scenario],
    workspace: directory, acpSessionId: 'same-native-session', auditLog, closeTimeoutMs: 200,
    env: { HYDRA_DIRECT_FIXTURE_JOURNAL: log },
  }), { mode: 0o600 });
  const transport = new StdioClientTransport({ command: process.execPath,
    args: process.argv[2] ? [process.argv[2], '--config', config]
      : scenario === 'packaged'
        ? [path.join(root, 'test/fixtures/scode-mcp-bundle.cjs'), path.join(root, 'dist/index.cjs'), config]
        : [path.join(root, 'dist/server.cjs'), '--config', config],
    env: { ...process.env, NODE_PATH: '' } as Record<string, string>, stderr: 'pipe',
  });
  let stderr = '';
  transport.stderr?.on('data', bytes => { stderr += String(bytes); });
  const client = new Client({ name: 'direct-smoke', version: '1' });
  try {
    await client.connect(transport);
    const tools = (await client.listTools()).tools.map(tool => tool.name).sort();
    assert.deepEqual(tools, ['scode_agent_list', 'scode_prompt', 'scode_send', 'scode_status']);
    if (scenario === 'task') {
      const task = await client.callTool({ name: 'scode_prompt', arguments: { text: 'Do a task' } });
      assert.ok(!task.isError);
      assert.match(JSON.stringify(task), /Task result/);
      assert.equal(journal(log).filter(row => row.method === 'session/prompt').length, 1);
      assert.equal(journal(log).filter(row => row.method === '_sudocode/a2a/send').length, 0);
      return;
    }
    const text = 'exact 零模型 🧪 text\nsecond\u0000line  ';
    const output = await client.callTool({ name: 'scode_send', arguments: { to: 'fixture-peer', message: text, summary: 'exact controller text' } });
    if (scenario === 'normal' || scenario === 'packaged') {
      assert.equal(output.isError, false);
      assert.ok(JSON.stringify(output).includes('bound-fixture-actor'));
      assert.equal(JSON.parse((output.content as Array<{text: string}>)[0].text).text, text,
        'MCP wraps the native string once without rewriting or double encoding');
      assert.equal((journal(log).find(row => row.method === '_sudocode/a2a/send')?.input as {message: string}).message, text);
      await client.callTool({ name: 'scode_agent_list', arguments: { activeOnly: false } });
      const forged = await client.callTool({ name: 'scode_send', arguments: { to: 'peer', message: 'bad', summary: 'bad sender', sender: 'other' } });
      assert.equal(forged.isError, true, 'unknown sender field must fail before forwarding');
      const structured = await client.callTool({ name: 'scode_send', arguments: { to: 'peer', message: { type: 'shutdown' }, summary: 'bad shape' } });
      assert.equal(structured.isError, true);
      const missingSummary = await client.callTool({ name: 'scode_send', arguments: { to: 'peer', message: 'missing summary' } });
      assert.equal(missingSummary.isError, true);
      const status = await client.callTool({ name: 'scode_status', arguments: {} });
      assert.match(JSON.stringify(status), /same-native-session/);
      assert.equal(new Set(journal(log).map(row => row.pid)).size, 1, 'all tools share one CLI');
      assert.equal(journal(log).filter(row => row.method === '_sudocode/a2a/send').length, 1);
      assert.equal(journal(auditLog).filter(row => row.phase === 'returned').length, 2);
      const audit = journal(auditLog).find(row => row.phase === 'returned')?.nativeAudit as Record<string, unknown>;
      assert.equal(audit.actor, 'bound-fixture-actor');
      assert.equal(audit.sessionId, 'same-native-session');
      assert.match(String(audit.commandId), /^external-a2a-/);
      assert.equal((output._meta as {sudocode: {directA2a: {audit: unknown}}}).sudocode.directA2a.audit !== undefined, true);
    } else {
      assert.equal(output.isError, true);
      assert.match(JSON.stringify(output), ['legacy', 'typed-refusal'].includes(scenario) ? /not_submitted/ : /unknown/);
      assert.equal(journal(log).filter(row => row.method === '_sudocode/a2a/send').length,
        scenario === 'legacy' ? 0 : 1, 'no fallback, duplicate request or blind retry');
      if (scenario === 'loss') {
        const status = await client.callTool({ name: 'scode_status', arguments: {} });
        assert.equal(JSON.parse((status.content as Array<{text: string}>)[0].text).closed, true,
          'a dead ACP child must not discard its pending Unknown response or respawn');
      }
    }
    assert.equal(journal(log).filter(row => row.method === 'session/prompt').length, 0);
    assert.equal(journal(log).filter(row => row.method === 'session/load').length, 1);
  } catch (error) { throw new Error(`${String(error)}\n${stderr}`); }
  finally { await client.close(); }
}

async function main(): Promise<void> {
  for (const scenario of ['normal', 'packaged', 'task', 'legacy', 'missing-method', 'remote-invalid-params',
    'typed-refusal', 'typed-unknown', 'unadvertised-refusal', 'bad-refusal', 'old-refusal-code', 'bad-audit', 'flipped-is-error', 'loss', 'malformed']) await checkMcp(scenario);
  // The same public helper rejects injected fields even outside the MCP schema.
  const log = path.join(directory, 'helper-fixture.jsonl');
  const connection = await connectScode({ scodeArgv: [process.execPath, fixture], workspace: directory,
    acpSessionId: 'helper-native', env: { HYDRA_DIRECT_FIXTURE_JOURNAL: log }, closeTimeoutMs: 200,
  });
  try {
    await assert.rejects(sendScodeMessage(connection, { to: 'peer', message: 'text', sender: 'other' } as never), /Unknown/);
    await assert.rejects(sendScodeMessage(connection, { to: 'peer', message: 'text' } as never), /summary/);
    assert.equal(journal(log).filter(row => row.method === '_sudocode/a2a/send').length, 0);
    const first = await sendScodeMessage(connection, { to: 'framed-peer', message: '零模型 🧪\0\n', summary: 'unicode' });
    const second = await sendScodeMessage(connection, { summary: 'unicode', message: '零模型 🧪\0\n', to: 'framed-peer' });
    assert.equal(scodeNativeA2aAudit(first)?.requestedInputSha256,
      '0334c65da7a90862a0effaa48677950e6f4c17a5b36f05b8939b9a4bc5c3f3aa');
    assert.equal(scodeNativeA2aAudit(first)?.requestedInputSha256, scodeNativeA2aAudit(second)?.requestedInputSha256);
  } finally { await connection.close(); }

  const notification = { sessionId: 'native', update: { sessionUpdate: 'user_message_chunk' as const,
    content: { type: 'text' as const, text: 'peer data' } } };
  for (const state of ['idle', 'active', 'notLoaded']) {
    const calls: Array<{ method: string; params: unknown }> = [];
    const request = async <T>(method: string, params: unknown): Promise<T> => {
      calls.push({ method, params });
      const result = method === 'thread/read' ? { thread: { id: 'original', status: { type: state } } }
        : method === 'thread/turns/list' ? { data: [{ id: 'running', status: 'inProgress' }] }
          : method === 'turn/steer' ? { turnId: 'running' } : { turn: { id: 'fresh' } };
      return result as T;
    };
    if (state === 'notLoaded') await assert.rejects(forwardScodeToCodex(request, 'original', notification, 'event'));
    else assert.equal((await forwardScodeToCodex(request, 'original', notification, 'event')).method,
      state === 'idle' ? 'turn/start' : 'turn/steer');
    assert.ok(!calls.some(call => /resume|fork/.test(call.method)));
    if (state === 'active') assert.equal((calls.at(-1)?.params as { expectedTurnId: string }).expectedTurnId, 'running');
  }
  console.log('PASS scode MCP: exact direct calls, one owner, no prompt fallback, strict inputs, response loss, audit and callback routing');
}
void main().catch(error => { console.error(error); process.exitCode = 1; }).finally(() => {
  // The smoke created and owns this exact disposable directory.
  assert.equal(path.dirname(directory), path.resolve(os.tmpdir()));
  assert.ok(path.basename(directory).startsWith('scode-acp-mcp-'));
  fs.rmSync(directory, { recursive: true, force: true });
});
