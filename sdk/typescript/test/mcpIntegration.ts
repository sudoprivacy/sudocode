import * as assert from 'node:assert/strict';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';
import { z } from 'zod';
const root = path.resolve(__dirname, '../..');
const { connectScode, sendScodeMessage, scodeNativeA2aAudit } = require(path.join(root, 'dist/index.cjs')) as typeof import('../src/index');
const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'scode-acp-mcp-'));
const fixture = path.join(root, 'test/fixtures/scode-direct-a2a-agent.mjs');
const journal = (file: string) => fs.existsSync(file)
  ? fs.readFileSync(file, 'utf8').trim().split('\n').filter(Boolean).map(line => JSON.parse(line) as Record<string, unknown>) : [];

async function ready(client: Client): Promise<void> {
  const deadline = Date.now() + 12000;
  while (Date.now() < deadline) {
    const result = await client.callTool({ name: 'scode_status', arguments: {} });
    if (JSON.parse((result.content as Array<{text: string}>)[0].text).closed === false) return;
    await new Promise(resolve => setTimeout(resolve, 30));
  }
  throw new Error('Original ACP session did not become ready');
}

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
    await client.connect(transport, { timeout: 5000 });
    const tools = (await client.listTools()).tools.map(tool => tool.name).sort();
    assert.deepEqual(tools, ['scode_agent_list', 'scode_prompt', 'scode_send', 'scode_status']);
    if (['delayed-initialize', 'closing-initialize'].includes(scenario)) {
      const deadline = Date.now() + 5000;
      while (!journal(log).some(row => row.method === 'initialize-pending') && Date.now() < deadline)
        await new Promise(resolve => setTimeout(resolve, 20));
      assert.equal(journal(log).filter(row => row.method === 'initialize-pending').length, 1);
      assert.equal(journal(log).filter(row => row.method === 'session/load').length, 0);
      const refused = await client.callTool({ name: 'scode_send', arguments: {
        to: 'fixture-peer', message: 'must not queue during startup', summary: 'not ready',
      } });
      assert.match(JSON.stringify(refused), /not_submitted/);
      assert.equal(journal(log).filter(row => row.method === '_sudocode/a2a/send').length, 0);
      if (scenario === 'closing-initialize') return;
      fs.writeFileSync(`${log}.initialize`, 'allow original initialization');
      await ready(client);
      const sent = await client.callTool({ name: 'scode_send', arguments: {
        to: 'fixture-peer', message: 'new explicit send after readiness', summary: 'ready',
      } });
      assert.equal(sent.isError, false);
      assert.equal(journal(log).filter(row => row.method === '_sudocode/a2a/send').length, 1);
      assert.equal(journal(log).filter(row => row.method === 'session/load').length, 1);
      assert.equal(journal(log).filter(row => row.method === 'session/new').length, 0);
      assert.equal(new Set(journal(log).map(row => row.pid)).size, 1);
      return;
    }
    if (scenario !== 'cold-recovery') await ready(client);
    if (scenario === 'task') {
      const task = await client.callTool({ name: 'scode_prompt', arguments: { text: 'Do a task' } });
      assert.ok(!task.isError);
      assert.match(JSON.stringify(task), /Task result/);
      assert.equal(journal(log).filter(row => row.method === 'session/prompt').length, 1);
      assert.equal(journal(log).filter(row => row.method === '_sudocode/a2a/send').length, 0);
      return;
    }
    const text = 'exact 零模型 🧪 text\nsecond\u0000line  ';
    if (scenario === 'cold-recovery') {
      const firstAttemptDeadline = Date.now() + 5000;
      while (!journal(log).some(row => row.method === 'initialize-offline') && Date.now() < firstAttemptDeadline)
        await new Promise(resolve => setTimeout(resolve, 20));
      assert.ok(journal(log).some(row => row.method === 'initialize-offline'));
      const refused = await client.callTool({ name: 'scode_send', arguments: { to: 'fixture-peer', message: text, summary: 'offline' } });
      assert.match(JSON.stringify(refused), /not_submitted/);
      fs.writeFileSync(`${log}.online`, 'ready');
      const deadline = Date.now() + 12000;
      while (journal(log).filter(row => row.method === 'session/load').length < 1 && Date.now() < deadline)
        await new Promise(resolve => setTimeout(resolve, 30));
      assert.equal(journal(log).filter(row => row.method === 'session/load').length, 1);
      await ready(client);
      const recovered = await client.callTool({ name: 'scode_send', arguments: { to: 'fixture-peer', message: text, summary: 'online' } });
      assert.equal(recovered.isError, false);
      assert.equal(journal(log).filter(row => row.method === '_sudocode/a2a/send').length, 1);
      assert.equal(journal(log).filter(row => row.method === 'session/new').length, 0);
      return;
    }
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
          'a dead ACP child preserves its pending Unknown response during backoff');
        const unavailable = await client.callTool({ name: 'scode_send', arguments: { to: 'fixture-peer', message: 'not sent', summary: 'offline' } });
        assert.match(JSON.stringify(unavailable), /not_submitted/);
        const deadline = Date.now() + 12000;
        while (journal(log).filter(row => row.method === 'session/load').length < 2 && Date.now() < deadline)
          await new Promise(resolve => setTimeout(resolve, 30));
        const recovered = await client.callTool({ name: 'scode_status', arguments: {} });
        assert.equal(JSON.parse((recovered.content as Array<{text: string}>)[0].text).closed, false);
        assert.equal(journal(log).filter(row => row.method === '_sudocode/a2a/send').length, 1, 'recovery never retries the ambiguous send');
        assert.equal(journal(log).filter(row => row.method === 'session/new').length, 0);
      }
    }
    assert.equal(journal(log).filter(row => row.method === 'session/prompt').length, 0);
    assert.equal(journal(log).filter(row => row.method === 'session/load').length, scenario === 'loss' ? 2 : 1);
  } catch (error) { throw new Error(`${String(error)}\n${stderr}`); }
  finally {
    await client.close();
    if (scenario === 'closing-initialize') {
      const pid = Number(journal(log).find(row => row.method === 'initialize-pending')?.pid);
      assert.ok(Number.isSafeInteger(pid) && pid > 0);
      const deadline = Date.now() + 5000;
      let exited = false;
      while (Date.now() < deadline) {
        try { process.kill(pid, 0); }
        catch (error) {
          if ((error as NodeJS.ErrnoException).code !== 'ESRCH') throw error;
          exited = true; break;
        }
        await new Promise(resolve => setTimeout(resolve, 20));
      }
      assert.ok(exited, 'Closing MCP during initialization must reap its ACP child');
      assert.equal(journal(log).filter(row => row.method === 'session/load').length, 0);
    }
  }
}

/** Claude Code channel: peers arrive as `notifications/claude/channel`, gated by policy. */
async function checkChannel(peerPolicy: 'node-namespace' | 'allowlist'): Promise<void> {
  const scenario = `channel-${peerPolicy}`;
  const config = path.join(directory, `${scenario}.json`);
  const log = path.join(directory, `${scenario}-fixture.jsonl`);
  const auditLog = path.join(directory, `${scenario}-audit.jsonl`);
  const identityGate = path.join(directory, `${scenario}-identity.ready`);
  fs.writeFileSync(config, JSON.stringify({ version: 1, scodeArgv: [process.execPath, fixture, scenario],
    workspace: directory, acpSessionId: 'channel-session', auditLog, closeTimeoutMs: 200,
    env: { HYDRA_DIRECT_FIXTURE_JOURNAL: log,
      ...(peerPolicy === 'node-namespace' ? { SCODE_DIRECT_FIXTURE_LIST_GATE: identityGate } : {}),
    },
    claudeChannel: { peerPolicy, ...(peerPolicy === 'allowlist' ? { peers: ['fixture-peer'] } : {}) },
  }), { mode: 0o600 });
  const transport = new StdioClientTransport({ command: process.execPath,
    args: process.argv[2] ? [process.argv[2], '--config', config] : [path.join(root, 'dist/server.cjs'), '--config', config],
    env: { ...process.env, NODE_PATH: '' } as Record<string, string>, stderr: 'pipe' });
  let stderr = '';
  transport.stderr?.on('data', bytes => { stderr += String(bytes); });
  const client = new Client({ name: 'channel-smoke', version: '1' });
  const events: Array<{ content: string; meta: Record<string, string> }> = [];
  client.setNotificationHandler(z.object({ method: z.literal('notifications/claude/channel'),
    params: z.object({ content: z.string(), meta: z.record(z.string(), z.string()) }) }), notification => {
    events.push(notification.params);
  });
  try {
    await client.connect(transport, { timeout: 5000 });
    assert.deepEqual(client.getServerCapabilities()?.experimental?.['claude/channel'], {});
    assert.match(String(client.getInstructions()), /reply only with the scode_send tool/);
    if (peerPolicy === 'node-namespace') {
      const pendingDeadline = Date.now() + 5000;
      while (journal(log).filter(row => row.method === 'peer-notification').length < 2 && Date.now() < pendingDeadline)
        await new Promise(resolve => setTimeout(resolve, 20));
      assert.equal(journal(log).filter(row => row.method === 'peer-notification').length, 2);
      await new Promise(resolve => setTimeout(resolve, 200));
      assert.equal(events.length, 0, 'MCP readiness must not bypass the Node identity check');
      fs.writeFileSync(identityGate, 'allow this connection identity check');
    }
    const deadline = Date.now() + 5000;
    const expected = peerPolicy === 'allowlist' ? 1 : 2;
    while (events.length < expected && Date.now() < deadline) await new Promise(resolve => setTimeout(resolve, 20));
    await new Promise(resolve => setTimeout(resolve, 200));
    assert.deepEqual(events.map(event => event.meta.from),
      peerPolicy === 'allowlist' ? ['fixture-peer'] : ['fixture-peer', 'stranger'], 'policy gates the sender');
    for (const event of events) {
      assert.match(event.content, /^<mailbox-message from="/);
      assert.equal(event.meta.verified, String(peerPolicy === 'node-namespace'),
        'only a checked Node identity makes `from` verified');
      assert.equal(event.meta.summary, 'greeting');
    }
    assert.equal(journal(log).filter(row => row.method === 'session/prompt').length, 0, 'no model turn for a peer');
    assert.equal(journal(auditLog).filter(row => row.phase === 'peer_message').length, expected);
  } catch (error) { throw new Error(`${String(error)}\n${stderr}`); }
  finally { await client.close(); }
}

async function main(): Promise<void> {
  // A focused regression control can supply the previous bridge executable.
  // Normal source and portable-package acceptance still run every case below.
  if (process.argv[3] !== undefined) {
    assert.equal(process.argv[3], '--case=delayed-initialize');
    await checkMcp('delayed-initialize');
    return;
  }
  for (const peerPolicy of ['node-namespace', 'allowlist'] as const) await checkChannel(peerPolicy);
  for (const scenario of ['normal', 'packaged', 'task', 'legacy', 'missing-method', 'remote-invalid-params',
    'typed-refusal', 'typed-unknown', 'unadvertised-refusal', 'bad-refusal', 'old-refusal-code', 'bad-audit', 'flipped-is-error', 'loss', 'cold-recovery', 'delayed-initialize', 'closing-initialize', 'malformed']) await checkMcp(scenario);
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

  console.log('PASS scode MCP: exact direct calls, one owner, no prompt fallback, strict inputs, response loss, audit, same-session recovery and Claude channel delivery');
}
void main().catch(error => { console.error(error); process.exitCode = 1; }).finally(() => {
  // The smoke created and owns this exact disposable directory.
  assert.equal(path.dirname(directory), path.resolve(os.tmpdir()));
  assert.ok(path.basename(directory).startsWith('scode-acp-mcp-'));
  fs.rmSync(directory, { recursive: true, force: true });
});
