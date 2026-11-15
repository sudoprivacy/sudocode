import { randomUUID } from 'node:crypto';
import { spawn } from 'node:child_process';
import * as assert from 'node:assert/strict';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';
import { CODEX_MAILBOX_DELIVERY_PROVENANCE } from '../src/codexMailboxWake';
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

const pause = (ms: number) => new Promise(resolve => setTimeout(resolve, ms));
async function until(predicate: () => boolean, ms: number, failure: string): Promise<void> {
  const deadline = Date.now() + ms;
  while (!predicate()) {
    if (Date.now() > deadline) throw new assert.AssertionError({ message: failure });
    await pause(20);
  }
}
const toolText = (result: unknown) => (result as { content: Array<{ text: string }> }).content[0].text;
interface Delivery { delivery: string; heads: unknown[]; text: string }

/** One Claude Code autojoin installation over the mailbox fixture: its bridges
 * are real `--claude` processes and its hooks real hook processes. Bridges run
 * on a test monotonic clock, so ten minutes pass without waiting. */
function claudeInstallation(name: string) {
  const state = path.join(directory, `${name}-state`), issuer = path.join(directory, `${name}-issuer`);
  fs.mkdirSync(issuer, { mode: 0o700 });
  const config = path.join(directory, `${name}.json`);
  fs.writeFileSync(config, JSON.stringify({ version: 1, mode: 'claude-autojoin', stateDirectory: state,
    auditLog: path.join(directory, `${name}-audit.jsonl`), peerPolicy: 'node-namespace',
    enrollmentArgv: [process.execPath, path.join(root, 'test/fixtures/codexAgentEnrollment.cjs'), issuer, 'mailbox'],
  }), { mode: 0o600 });
  const server = process.argv[2] ?? path.join(root, 'dist/server.cjs');
  const unread = path.join(issuer, 'unread'), clock = path.join(issuer, 'clock');
  fs.writeFileSync(clock, '0');
  return {
    state, issuer, config,
    spool: (sessionId: string) => path.join(state, `${sessionId}.wake.jsonl`),
    deliveries: (sessionId: string) => journal(path.join(state, `${sessionId}.wake.jsonl`)) as unknown as Delivery[],
    claims: () => journal(path.join(issuer, 'claims.jsonl')) as Array<{ actor: string; threadId: string; workspace: string }>,
    acp: () => journal(path.join(issuer, 'acp.jsonl')) as Array<{ method: string; params?: any; response?: any }>,
    /** scode reports the unread count again, as on a new peer frame. */
    unread: () => fs.appendFileSync(unread, '.'),
    advance: (ms: number) => fs.writeFileSync(clock, String(Number(fs.readFileSync(clock, 'utf8')) + ms)),
    async bridge(sessionId: string, entrypoint = 'cli') {
      const transport = new StdioClientTransport({ command: process.execPath,
        args: ['--require', path.join(root, 'test/fixtures/monotonicClock.cjs'), server, '--claude', '--config', config],
        env: { ...process.env, NODE_PATH: '', CLAUDE_CODE_ENTRYPOINT: entrypoint, CLAUDE_CODE_SESSION_ID: sessionId,
          CLAUDE_PROJECT_DIR: directory, SCODE_FIXTURE_CLOCK: clock, SCODE_DIRECT_FIXTURE_UNREAD: unread } as Record<string, string>,
        stderr: 'pipe' });
      let stderr = ''; transport.stderr?.on('data', bytes => { stderr += String(bytes); });
      const client = new Client({ name: `claude-${name}`, version: '1' });
      await client.connect(transport);
      return { client, transport, stderr: () => stderr,
        status: async () => JSON.parse(toolText(await client.callTool({ name: 'scode_status', arguments: {} }))) };
    },
    hook: (flag: '--claude-watch' | '--claude-deliver', input: object, entrypoint = 'cli') =>
      new Promise<{ code: number | null; stdout: string; stderr: string }>(resolve => {
        const child = spawn(process.execPath, [server, flag, '--config', config], { stdio: ['pipe', 'pipe', 'pipe'],
          env: { ...process.env, CLAUDE_CODE_ENTRYPOINT: entrypoint } });
        let stdout = '', stderr = '';
        child.stdout!.on('data', bytes => { stdout += String(bytes); });
        child.stderr!.on('data', bytes => { stderr += String(bytes); });
        child.on('close', code => resolve({ code, stdout, stderr }));
        child.stdin!.end(JSON.stringify(input));
      }),
  };
}

/** The SessionStart hook names the session's spool; the FileChanged hook hands
 * each complete entry over exactly once and ignores every other change. */
async function checkClaudeHooks(): Promise<void> {
  const claude = claudeInstallation('claude-hooks');
  const sessionId = randomUUID(), spool = claude.spool(sessionId);
  const nothing = { code: 0, stdout: '', stderr: '' };
  assert.deepEqual(await claude.hook('--claude-watch', { session_id: sessionId, hook_event_name: 'SessionStart', source: 'startup' }),
    { code: 0, stdout: JSON.stringify({ hookSpecificOutput: { hookEventName: 'SessionStart', watchPaths: [spool] } }) + '\n', stderr: '' });
  assert.equal(fs.readFileSync(spool, 'utf8'), '', 'the watched spool exists before the first delivery');
  if (process.platform !== 'win32') assert.equal(fs.statSync(spool).mode & 0o777, 0o600, 'the spool is private');
  const other = randomUUID();
  assert.deepEqual(await claude.hook('--claude-watch', { session_id: other }, 'sdk-cli'), nothing, 'a session without an Agent watches nothing');
  assert.ok(!fs.existsSync(claude.spool(other)));
  const changed = (event = 'change', file = spool) => ({ session_id: sessionId, hook_event_name: 'FileChanged', file_path: file, event });
  const deliver = (input: object = changed(), entrypoint?: string) => claude.hook('--claude-deliver', input, entrypoint);
  const entry = (text: string) => fs.appendFileSync(spool, JSON.stringify({ delivery: `${'0'.repeat(64)}:1`, heads: [], text }) + '\n');
  assert.deepEqual(await deliver(), nothing, 'a change without a new entry hands nothing over');
  // Overlapping runs for one change hand each entry over exactly once.
  for (let round = 0; round < 5; round += 1) {
    entry(`delivery ${round} 🧪`);
    const runs = await Promise.all([deliver(), deliver(), deliver()]);
    assert.deepEqual(runs.filter(run => run.code === 2).map(run => run.stderr), [`delivery ${round} 🧪\n`], `round ${round}: one claim`);
    assert.ok(runs.every(run => run.code === 2 || (run.code === 0 && !run.stderr && !run.stdout)), JSON.stringify(runs));
  }
  entry('second'); entry('third');
  fs.appendFileSync(spool, '{"delivery":"partial');
  assert.deepEqual(await deliver(changed('unlink')), nothing, 'a removal hands nothing over');
  assert.deepEqual(await deliver(changed('change', spool + '.cursor')), nothing, 'another path hands nothing over');
  assert.deepEqual(await deliver(changed(), 'sdk-cli'), nothing, 'a session without an Agent hands nothing over');
  assert.deepEqual(await deliver(changed('add')), { code: 2, stdout: '', stderr: 'second\n\nthird\n' },
    'every complete new entry, and no entry still being written');
  fs.appendFileSync(spool, ':1","heads":[],"text":"completed"}\n');
  assert.deepEqual(await deliver(), { code: 2, stdout: '', stderr: 'completed\n' });
  assert.deepEqual(await deliver(), nothing);
  // Mailbox offers are not filtered by sender: an allowlist is refused.
  const allowlist = path.join(directory, 'claude-allowlist.json');
  fs.writeFileSync(allowlist, JSON.stringify({ ...JSON.parse(fs.readFileSync(claude.config, 'utf8')), peerPolicy: 'allowlist',
    peers: ['fixture-peer'] }), { mode: 0o600 });
  const refused = await new Promise<{ code: number | null; stderr: string }>(resolve => {
    const child = spawn(process.execPath, [process.argv[2] ?? path.join(root, 'dist/server.cjs'), '--claude-watch', '--config', allowlist],
      { stdio: ['pipe', 'ignore', 'pipe'], env: { ...process.env, CLAUDE_CODE_ENTRYPOINT: 'cli' } });
    let stderr = ''; child.stderr!.on('data', bytes => { stderr += String(bytes); });
    child.on('close', code => resolve({ code, stderr }));
    child.stdin!.end(JSON.stringify({ session_id: sessionId }));
  });
  assert.equal(refused.code, 1);
  assert.match(refused.stderr, /peerPolicy node-namespace/);
}

/** The bridge delivers an unread frame under its budget: one spool entry per
 * delivery, at most three per key, ten minutes apart, a new key when the
 * session calls a bridge tool, and nothing after the model's acknowledgement. */
async function checkClaudeDelivery(): Promise<void> {
  const claude = claudeInstallation('claude-delivery');
  const sessionId = randomUUID();
  const entries = () => claude.deliveries(sessionId);
  const quiet = async (count: number, failure: string) => { await pause(500); assert.equal(entries().length, count, failure); };
  const bridge = await claude.bridge(sessionId);
  try {
    // No bridge tool has been called yet.
    await until(() => entries().length === 1, 10000, 'an unread frame is delivered once the bridge attaches');
    const [first] = entries();
    assert.equal(first.text, `${CODEX_MAILBOX_DELIVERY_PROVENANCE}\nPeer body 🧪\n⟦ack fixture-ack⟧`);
    assert.deepEqual(first.heads, [{ conversation: 'conversation', start: 0, deliveryId: 'delivery-one' }]);
    assert.match(first.delivery, /^[0-9a-f]{64}:1$/);
    const offers = claude.acp().filter(row => row.method === 'peer_offer');
    assert.ok(offers.length && offers.every(row => row.params.threadId === sessionId && !row.params.ack),
      'the session is the mailbox thread, and a delivery acknowledges nothing');
    assert.ok(offers[0].params.maxBytes + CODEX_MAILBOX_DELIVERY_PROVENANCE.length + 1 <= 10000,
      'a delivery fits the hook output Claude Code adds to its context');
    assert.deepEqual(await claude.hook('--claude-deliver', { session_id: sessionId, hook_event_name: 'FileChanged',
      file_path: claude.spool(sessionId), event: 'change' }), { code: 2, stdout: '', stderr: first.text + '\n' });
    claude.advance(9 * 60_000); claude.unread();
    await quiet(1, 'no redelivery within ten minutes');
    for (const n of [2, 3]) {
      claude.advance(n === 2 ? 60_000 : 10 * 60_000); claude.unread();
      await until(() => entries().length === n, 5000, `delivery ${n} follows ten minutes after the previous one`);
      assert.deepEqual(entries()[n - 1], { ...first, delivery: first.delivery.replace(/:1$/, `:${n}`) }, 'the same key spends its next number');
    }
    claude.advance(10 * 60_000); claude.unread();
    await quiet(3, 'three deliveries exhaust the key');
    // This call is itself a bridge tool call: it renews the epoch.
    const exhausted = (await bridge.status()).delivery;
    assert.deepEqual(exhausted, { state: 'delivery_exhausted', deliveryId: 'delivery-one', submissions: 3 });
    claude.unread();
    await until(() => entries().length === 4, 5000, 'a bridge tool call renews the epoch');
    const renewed = entries()[3];
    assert.match(renewed.delivery, /^[0-9a-f]{64}:1$/);
    assert.notEqual(renewed.delivery.split(':')[0], first.delivery.split(':')[0], 'a new epoch makes a new key');
    assert.equal(journal(path.join(claude.state, `${sessionId}.delivery.jsonl`)).length, 4, 'each delivery was logged');
    // Only the model's acknowledgement retires the frame.
    assert.equal(toolText(await bridge.client.callTool({ name: 'scode_inbox', arguments: { ack: ['fixture-ack'] } })),
      'No unread mailbox frames');
    assert.deepEqual(claude.acp().filter(row => row.method === 'peer_offer_result' && row.response.commits.length)
      .map(row => row.response.commits), [[{ conversation: 'conversation', from: 0, to: 100, status: 'committed' }]]);
    claude.advance(10 * 60_000); claude.unread();
    await quiet(4, 'an acknowledged frame is not delivered again');
    assert.equal((await bridge.status()).delivery.state, 'idle');
    assert.equal(claude.acp().filter(row => row.method === 'session/prompt').length, 0, 'no turn of the scode session');
  } catch (error) { throw new Error(`${String(error)}\n${bridge.stderr()}`); }
  finally { await bridge.client.close(); }
}

/** Two Claude Code processes on one session: exactly one bridge delivers, and
 * the other takes over when the first is killed. */
async function checkClaudeOwners(): Promise<void> {
  const claude = claudeInstallation('claude-owners');
  const sessionId = randomUUID();
  const entries = () => claude.deliveries(sessionId);
  const owners = path.join(claude.state, `${sessionId}.delivery.owner`);
  const first = await claude.bridge(sessionId);
  let second: Awaited<ReturnType<typeof claude.bridge>> | undefined;
  try {
    await until(() => entries().length === 1, 10000, 'the first bridge delivers');
    second = await claude.bridge(sessionId);
    let standby: Record<string, any> = {};
    const deadline = Date.now() + 10000;
    while (Date.now() < deadline) {
      standby = await second.status();
      if (standby.closed === false) break;
      await pause(50);
    }
    assert.deepEqual(standby.delivery, { state: 'standby', detail: 'another bridge owns this session\'s deliveries',
      owner: first.transport.pid });
    claude.advance(10 * 60_000); claude.unread();
    await until(() => entries().length === 2, 5000, 'the owner delivers again after ten minutes');
    await pause(500);
    assert.equal(entries().length, 2, 'exactly one bridge delivers');
    process.kill(first.transport.pid!, 'SIGKILL');
    await until(() => entries().length === 3, 15000, 'the standing-by bridge takes over from a killed owner');
    await pause(500);
    assert.equal(entries().length, 3);
    assert.equal((await second.status()).delivery.state, 'submitted', 'the new owner continues the same log');
    const generations = fs.readdirSync(owners).filter(name => /^[0-9]+$/.test(name));
    assert.deepEqual(generations, ['2'], 'the takeover created the next claim generation');
    assert.equal(JSON.parse(fs.readFileSync(path.join(owners, '2'), 'utf8')).pid, second.transport.pid);
  } catch (error) { throw new Error(`${String(error)}\n${first.stderr()}\n${second?.stderr() ?? ''}`); }
  finally { await first.client.close().catch(() => {}); await second?.client.close(); }
}

/** Claude Code autojoin: each session enrolls its own claude-<hex> Agent once,
 * resumes it by session id, and receives deliveries through its own spool. */
async function checkClaudeAutojoin(): Promise<void> {
  const claude = claudeInstallation('claude-autojoin');
  const session = async (sessionId: string) => {
    const before = claude.deliveries(sessionId).length;
    const bridge = await claude.bridge(sessionId);
    try {
      await until(() => claude.deliveries(sessionId).length > before, 10000, 'the unread frame reaches this session\'s own spool');
      await pause(200);
      assert.equal(claude.deliveries(sessionId).length, before + 1);
    } catch (error) { throw new Error(`${String(error)}\n${bridge.stderr()}`); }
    finally { await bridge.client.close(); }
  };
  const first = randomUUID(), second = randomUUID();
  await session(first);
  assert.equal(claude.claims().length, 1);
  assert.match(claude.claims()[0].actor, /^claude-[0-9a-f]{32}$/, 'a Claude Code session enrolls a claude-<hex> Agent');
  assert.equal(claude.claims()[0].threadId, first, 'the session id is the native root');
  await session(first);
  assert.equal(claude.claims().length, 1, 'resuming a session reuses its original Agent and ACP session');
  await session(second);
  assert.equal(claude.claims().length, 2);
  assert.notEqual(claude.claims()[1].actor, claude.claims()[0].actor, 'each session gets its own Agent');
  // A print-mode session has no user to address: no Agent, no tools, no spool.
  const scripted = randomUUID();
  const print = await claude.bridge(scripted, 'sdk-cli');
  try {
    assert.equal(print.client.getServerCapabilities()?.tools, undefined, 'a print-mode session runs without an Agent or tools');
  } finally { await print.client.close(); }
  assert.equal(claude.claims().length, 2, 'a print-mode session enrolls nothing');
  assert.ok(!fs.readdirSync(claude.state).some(file => file.startsWith(scripted)));
  // A session that starts while its Node is unreachable joins once it returns, without a restart.
  const outaged = randomUUID();
  fs.writeFileSync(path.join(claude.issuer, 'outage'), '');
  const outage = await claude.bridge(outaged);
  try {
    const refused = JSON.stringify(await outage.client.callTool({ name: 'scode_status', arguments: {} }));
    assert.match(refused, /not_submitted/);
    assert.match(refused, /Not attached to the agent network yet/);
    assert.equal(claude.claims().length, 2, 'an unreachable Node issues nothing');
    fs.rmSync(path.join(claude.issuer, 'outage'));
    await until(() => claude.deliveries(outaged).length === 1, 10000, 'the session attaches in the background and receives its frame');
    assert.equal(claude.claims().length, 3, 'the retried operation issues exactly once');
    assert.match(JSON.stringify(await outage.client.callTool({ name: 'scode_status', arguments: {} })), /fixture-/);
  } catch (error) { throw new Error(`${String(error)}\n${outage.stderr()}`); }
  finally { await outage.client.close(); }
  assert.ok(!fs.readdirSync(claude.state).some(file => file.endsWith('.mcp.json')), 'no derived session config is persisted');
  // The hook names its session through Claude Code's JSON input.
  const changed = { session_id: first, hook_event_name: 'FileChanged', file_path: claude.spool(first), event: 'change' };
  assert.deepEqual(await claude.hook('--claude-deliver', changed, 'sdk-cli'), { code: 0, stdout: '', stderr: '' },
    'a session without an Agent hands nothing over');
  const woke = await claude.hook('--claude-deliver', changed);
  assert.equal(woke.code, 2);
  assert.equal(woke.stderr.split(CODEX_MAILBOX_DELIVERY_PROVENANCE).length - 1, 2,
    'both deliveries to this session wake it, and none of another session\'s');
}

async function main(): Promise<void> {
  // A focused regression control can supply the previous bridge executable.
  // Normal source and portable-package acceptance still run every case below.
  if (process.argv[3] !== undefined) {
    assert.equal(process.argv[3], '--case=delayed-initialize');
    await checkMcp('delayed-initialize');
    return;
  }
  await checkClaudeHooks();
  await checkClaudeDelivery();
  await checkClaudeOwners();
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

  console.log('PASS scode MCP: exact direct calls, one owner, no prompt fallback, strict inputs, response loss, audit, same-session recovery and Claude Code mailbox delivery');
}
void main().catch(error => { console.error(error); process.exitCode = 1; }).finally(() => {
  // The smoke created and owns this exact disposable directory.
  assert.equal(path.dirname(directory), path.resolve(os.tmpdir()));
  assert.ok(path.basename(directory).startsWith('scode-acp-mcp-'));
  fs.rmSync(directory, { recursive: true, force: true });
});
