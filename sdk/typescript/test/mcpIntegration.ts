import { randomUUID } from 'node:crypto';
import { spawn } from 'node:child_process';
import * as assert from 'node:assert/strict';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';
import { z } from 'zod';
const root = path.resolve(__dirname, '../..');
const { connectScode, sendScodeMessage, scodeNativeA2aAudit, CLAUDE_WAKE_HOOK_TIMEOUT_SECONDS } = require(path.join(root, 'dist/index.cjs')) as typeof import('../src/index');
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
      await client.callTool({ name: 'scode_agent_list', arguments: {} });
      const activeOnly = await client.callTool({ name: 'scode_agent_list', arguments: { activeOnly: true } });
      assert.equal(activeOnly.isError, true, 'peers have no liveness to filter on; the bridge refuses activeOnly');
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
        // The child journals session/load before its reply reaches the MCP
        // supervisor. Wait for public readiness, under the same 12 s bound.
        await ready(client);
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

/** Claude Code wake: the bridge spools peers and the asyncRewake hook waiter hands them over with exit 2. */
async function checkWake(): Promise<void> {
  const config = path.join(directory, 'wake.json');
  const log = path.join(directory, 'wake-fixture.jsonl');
  const auditLog = path.join(directory, 'wake-audit.jsonl');
  const spool = path.join(directory, 'wake-spool.jsonl');
  fs.writeFileSync(config, JSON.stringify({ version: 1, scodeArgv: [process.execPath, fixture, 'channel-allowlist'],
    workspace: directory, acpSessionId: 'channel-session', auditLog, closeTimeoutMs: 200,
    env: { HYDRA_DIRECT_FIXTURE_JOURNAL: log },
    claudeChannel: { peerPolicy: 'allowlist', peers: ['fixture-peer'], wakeSpool: spool },
  }), { mode: 0o600 });
  const server = process.argv[2] ?? path.join(root, 'dist/server.cjs');
  const transport = new StdioClientTransport({ command: process.execPath, args: [server, '--config', config],
    env: { ...process.env, NODE_PATH: '' } as Record<string, string>, stderr: 'pipe' });
  const client = new Client({ name: 'wake-smoke', version: '1' });
  let notifications = 0;
  client.setNotificationHandler(z.object({ method: z.literal('notifications/claude/channel'), params: z.any() }),
    () => { notifications += 1; });
  const waiters: Array<ReturnType<typeof spawn>> = [];
  const arm = () => {
    const child = spawn(process.execPath, [server, '--claude-wake', '--config', config], { stdio: ['ignore', 'ignore', 'pipe'] });
    waiters.push(child);
    let stderr = ''; child.stderr!.on('data', bytes => { stderr += String(bytes); });
    return { child, exited: new Promise<{ code: number | null; stderr: string }>(resolve =>
      child.on('exit', code => resolve({ code, stderr }))) };
  };
  const wake = () => arm().exited;
  const peer = (text: string) => fs.appendFileSync(spool, JSON.stringify({ at: new Date().toISOString(),
    from: 'fixture-peer', verified: true, text: `<mailbox-message from="fixture-peer">${text}</mailbox-message>` }) + '\n');
  const pause = (ms: number) => new Promise(resolve => setTimeout(resolve, ms));
  const within = <T>(promise: Promise<T>, ms: number, failure: string) => Promise.race([promise,
    pause(ms).then(() => { throw new assert.AssertionError({ message: failure }); })]);
  try {
    await client.connect(transport);
    const deadline = Date.now() + 5000;
    while (!(fs.existsSync(spool) && fs.readFileSync(spool, 'utf8').trim()) && Date.now() < deadline) {
      await new Promise(resolve => setTimeout(resolve, 20));
    }
    await new Promise(resolve => setTimeout(resolve, 200));
    const spooled = fs.readFileSync(spool, 'utf8').trim().split('\n').map(line => JSON.parse(line));
    assert.deepEqual(spooled.map(entry => entry.from), ['fixture-peer'], 'the allowlist still gates spooled peers');
    assert.equal(notifications, 0, 'a wake spool replaces channel notifications');
    if (process.platform !== 'win32') assert.equal(fs.statSync(spool).mode & 0o777, 0o600, 'the spool is private');
    const first = await wake();
    assert.equal(first.code, 2, 'pending peers wake Claude');
    assert.match(first.stderr, /^<channel source="nexus_scode" from="fixture-peer" verified="false" kind="message" summary="greeting">\n<mailbox-message from="/);
    // The waiter armed by the latest turn covers the session; an earlier one returns without delivering.
    const earlier = wake();
    await pause(300);
    const newest = wake();
    const superseded = await within(earlier, 3000, 'a newer waiter takes over');
    assert.equal(superseded.code, 0, 'a newer waiter takes over');
    assert.equal(superseded.stderr, '');
    peer('later');
    const later = await newest;
    assert.equal(later.code, 2);
    assert.doesNotMatch(later.stderr, /greeting/, 'delivered peers are not repeated');
    assert.match(later.stderr, /verified="true">\n<mailbox-message from="fixture-peer">later/);
    // Claude Code ends a waiter at its hook timeout: the next waiter delivers what arrived meanwhile.
    const ended = arm();
    await pause(300);
    ended.child.kill('SIGKILL');
    await ended.exited;
    peer('while-unarmed');
    const next = await wake();
    assert.equal(next.code, 2);
    assert.match(next.stderr, /while-unarmed/);
    assert.doesNotMatch(next.stderr, /later/);
    // Waiters overlapping in a handover hand each peer over exactly once.
    for (let round = 0; round < 5; round += 1) {
      peer(`overlap-${round}`);
      const overlapping = [arm(), arm(), arm()];
      const results: Array<{ code: number | null; stderr: string }> = [];
      for (const waiter of overlapping) void waiter.exited.then(result => results.push(result));
      const deadline = Date.now() + 5000;
      while (!results.some(result => result.code === 2) && Date.now() < deadline) await pause(20);
      await pause(1200);
      // A waiter that lost the overlap keeps waiting for the next peer; release it.
      if (results.length < overlapping.length) peer(`release-${round}`);
      const all = await Promise.all(overlapping.map(waiter => waiter.exited));
      const delivered = all.map(result => result.stderr).join('');
      assert.equal(delivered.match(new RegExp(`overlap-${round}<`, 'g'))?.length, 1, `round ${round}: one delivery`);
      assert.ok((delivered.match(new RegExp(`release-${round}<`, 'g'))?.length ?? 0) <= 1);
      assert.ok(all.every(result => result.code === 0 || result.code === 2), JSON.stringify(all));
    }
    // The documented hook entry carries the timeout the waiter plans around.
    assert.match(fs.readFileSync(path.join(root, 'README.md'), 'utf8'),
      new RegExp(`"asyncRewake": true, "timeout": ${CLAUDE_WAKE_HOOK_TIMEOUT_SECONDS},`));
    assert.equal(journal(log).filter(row => row.method === 'session/prompt').length, 0, 'no model turn for a peer');
    assert.equal(journal(auditLog).filter(row => row.phase === 'peer_message' && row.delivery === 'wake').length, 1);
  } finally {
    // A failed check must not leave a waiter polling the fixture spool.
    for (const child of waiters) if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL');
    await client.close();
  }
}

/** Claude Code autojoin: each session enrolls its own claude-<hex> Agent once,
 * resumes it by session id, and wakes through its own spool. */
async function checkClaudeAutojoin(): Promise<void> {
  const state = path.join(directory, 'claude-autojoin');
  const issuer = path.join(directory, 'claude-issuer');
  fs.mkdirSync(issuer, { mode: 0o700 });
  const config = path.join(directory, 'claude-autojoin.json');
  fs.writeFileSync(config, JSON.stringify({ version: 1, mode: 'claude-autojoin', stateDirectory: state,
    auditLog: path.join(directory, 'claude-autojoin-audit.jsonl'), peerPolicy: 'allowlist', peers: ['fixture-peer'],
    enrollmentArgv: [process.execPath, path.join(root, 'test/fixtures/codexAgentEnrollment.cjs'), issuer, 'channel-allowlist'],
  }), { mode: 0o600 });
  const server = process.argv[2] ?? path.join(root, 'dist/server.cjs');
  const claims = () => fs.existsSync(path.join(issuer, 'claims.jsonl'))
    ? journal(path.join(issuer, 'claims.jsonl')) as Array<{ actor: string; threadId: string; workspace: string }> : [];
  const session = async (sessionId: string) => {
    const transport = new StdioClientTransport({ command: process.execPath, args: [server, '--claude', '--config', config],
      env: { ...process.env, NODE_PATH: '', CLAUDE_CODE_ENTRYPOINT: 'cli', CLAUDE_CODE_SESSION_ID: sessionId,
        CLAUDE_PROJECT_DIR: directory } as Record<string, string>,
      stderr: 'pipe' });
    let stderr = ''; transport.stderr?.on('data', bytes => { stderr += String(bytes); });
    const client = new Client({ name: 'claude-autojoin', version: '1' });
    const spool = path.join(state, `${sessionId}.wake.jsonl`);
    const before = fs.existsSync(spool) ? fs.readFileSync(spool, 'utf8').split('\n').filter(Boolean).length : 0;
    try {
      await client.connect(transport);
      const deadline = Date.now() + 5000;
      const count = () => fs.existsSync(spool) ? fs.readFileSync(spool, 'utf8').split('\n').filter(Boolean).length : 0;
      while (count() <= before && Date.now() < deadline) await new Promise(resolve => setTimeout(resolve, 20));
      assert.equal(count(), before + 1, 'the allowlisted peer reaches this session\'s own spool');
    } catch (error) { throw new Error(`${String(error)}\n${stderr}`); }
    finally { await client.close(); }
  };
  const first = randomUUID(), second = randomUUID();
  await session(first);
  assert.equal(claims().length, 1);
  assert.match(claims()[0].actor, /^claude-[0-9a-f]{32}$/, 'a Claude Code session enrolls a claude-<hex> Agent');
  assert.equal(claims()[0].threadId, first, 'the session id is the native root');
  await session(first);
  assert.equal(claims().length, 1, 'resuming a session reuses its original Agent and ACP session');
  await session(second);
  assert.equal(claims().length, 2);
  assert.notEqual(claims()[1].actor, claims()[0].actor, 'each session gets its own Agent');
  // A print-mode session has no user to address: no Agent, no tools, no spool.
  const scripted = randomUUID();
  const printTransport = new StdioClientTransport({ command: process.execPath, args: [server, '--claude', '--config', config],
    env: { ...process.env, NODE_PATH: '', CLAUDE_CODE_ENTRYPOINT: 'sdk-cli', CLAUDE_CODE_SESSION_ID: scripted,
      CLAUDE_PROJECT_DIR: directory } as Record<string, string>, stderr: 'pipe' });
  const printClient = new Client({ name: 'claude-print', version: '1' });
  try {
    await printClient.connect(printTransport);
    assert.equal(printClient.getServerCapabilities()?.tools, undefined, 'a print-mode session runs without an Agent or tools');
  } finally { await printClient.close(); }
  assert.equal(claims().length, 2, 'a print-mode session enrolls nothing');
  assert.ok(!fs.existsSync(path.join(state, `${scripted}.wake.jsonl`)));
  // A session that starts while its Node is unreachable joins once it returns, without a restart.
  const outaged = randomUUID();
  fs.writeFileSync(path.join(issuer, 'outage'), '');
  const outageTransport = new StdioClientTransport({ command: process.execPath, args: [server, '--claude', '--config', config],
    env: { ...process.env, NODE_PATH: '', CLAUDE_CODE_ENTRYPOINT: 'cli', CLAUDE_CODE_SESSION_ID: outaged,
      CLAUDE_PROJECT_DIR: directory } as Record<string, string>, stderr: 'pipe' });
  let outageStderr = ''; outageTransport.stderr?.on('data', bytes => { outageStderr += String(bytes); });
  const outageClient = new Client({ name: 'claude-outage', version: '1' });
  try {
    await outageClient.connect(outageTransport);
    const refused = JSON.stringify(await outageClient.callTool({ name: 'scode_status', arguments: {} }));
    assert.match(refused, /not_submitted/);
    assert.match(refused, /Not attached to the agent network yet/);
    assert.equal(claims().length, 2, 'an unreachable Node issues nothing');
    fs.rmSync(path.join(issuer, 'outage'));
    const spool = path.join(state, `${outaged}.wake.jsonl`);
    const deadline = Date.now() + 10000;
    while (!fs.existsSync(spool) && Date.now() < deadline) await new Promise(resolve => setTimeout(resolve, 50));
    assert.ok(fs.existsSync(spool), 'the session attaches in the background and receives its peer');
    assert.equal(claims().length, 3, 'the retried operation issues exactly once');
    assert.match(JSON.stringify(await outageClient.callTool({ name: 'scode_status', arguments: {} })), /fixture-/);
  } catch (error) { throw new Error(`${String(error)}\n${outageStderr}`); }
  finally { await outageClient.close(); }
  assert.ok(!fs.readdirSync(state).some(file => file.endsWith('.mcp.json')), 'no derived session config is persisted');
  // The hook names its session through Claude Code's JSON input.
  const wake = (entrypoint: string) => new Promise<{ code: number | null; stderr: string }>(resolve => {
    const child = spawn(process.execPath, [server, '--claude-wake', '--config', config], { stdio: ['pipe', 'ignore', 'pipe'],
      env: { ...process.env, CLAUDE_CODE_ENTRYPOINT: entrypoint } });
    let stderr = ''; child.stderr!.on('data', bytes => { stderr += String(bytes); });
    child.on('exit', code => resolve({ code, stderr }));
    child.stdin!.end(JSON.stringify({ session_id: first, hook_event_name: 'Stop' }));
  });
  const unarmed = await wake('sdk-cli');
  assert.equal(unarmed.code, 0, 'a session without an Agent never arms a waiter');
  assert.equal(unarmed.stderr, '');
  const woke = await wake('cli');
  assert.equal(woke.code, 2);
  assert.equal((woke.stderr.match(/<channel source="nexus_scode" from="fixture-peer"/g) ?? []).length, 2,
    'both peers spooled for this session wake it, and none of the other session\'s');
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
  await checkWake();
  await checkClaudeAutojoin();
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
