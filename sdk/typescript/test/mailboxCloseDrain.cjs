// Process-level regression: unload a native root while its ACP ACK is in flight.
// The native endpoint and ACP contents are fixtures; the SDK owner and child
// lifetime are real. This does not qualify a native executable or live binding.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { once } = require('node:events');
const { WebSocketServer } = require('ws');

const sdk = process.argv[3] ?? path.resolve(__dirname, '..');
const library = process.argv[2] ?? path.join(sdk, 'dist/index.cjs');
const agentFixture = process.argv[4] ?? path.join(sdk, 'test/fixtures/scode-direct-a2a-agent.mjs');
const { connectCodexAutojoin } = require(library);
const threadId = '33333333-3333-3333-3333-333333333333';
const pause = ms => new Promise(resolve => setTimeout(resolve, ms));
const rows = file => fs.existsSync(file) ? fs.readFileSync(file, 'utf8').split('\n').filter(Boolean).map(JSON.parse) : [];

async function main() {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'scode-ack-drain-'));
  fs.chmodSync(directory, 0o700);
  const journal = path.join(directory, 'acp.jsonl'), barrier = path.join(directory, 'release-ack');
  const endpoint = new WebSocketServer({ host: '127.0.0.1', port: 0 });
  await once(endpoint, 'listening');
  const queue = [];
  let loaded = true, nativeFailure, owner, ackTask, unloadTask;
  endpoint.on('connection', socket => socket.on('message', bytes => {
    try {
      const call = JSON.parse(bytes.toString());
      if (call.id === undefined) return;
      let result;
      if (call.method === 'initialize') result = { userAgent: 'private-ack-drain-fixture' };
      else if (call.method === 'thread/loaded/list') result = { data: loaded ? [threadId] : [], nextCursor: null };
      else if (call.method === 'thread/read') result = { thread: { id: threadId, cwd: directory,
        status: { type: 'idle' }, parentThreadId: null, threadSource: 'user', ephemeral: false, canAcceptDirectInput: true, turns: [] } };
      else if (call.method === 'thread/turns/list') result = { data: [], nextCursor: null };
      else if (call.method === 'thread/queue/list') result = { data: queue, nextCursor: null };
      else if (call.method === 'thread/queue/add') {
        queue.push({ id: 'queue-one', clientUserMessageId: call.params.clientUserMessageId });
        result = { queuedSubmission: queue.at(-1) };
      } else throw new Error('Unexpected native method '+call.method);
      socket.send(JSON.stringify({ jsonrpc: '2.0', id: call.id, result }));
    } catch (error) { nativeFailure = error; socket.terminate(); }
  }));
  try {
    const nativeEnv = { ...process.env, SCODE_NATIVE_ENV_SENTINEL: 'owned-proxy' };
    delete nativeEnv.CODEX_THREAD_ID;
    owner = await connectCodexAutojoin({
      native: { codexArgv: [process.execPath, path.join(sdk, 'test/fixtures/codex-stdio-proxy.mjs'),
        String(endpoint.address().port), 'normal'], socket: 'fixture-native.sock', closeTimeoutMs: 900,
        env: nativeEnv },
      stateDirectory: path.join(directory, 'state'), peers: [], peerPolicy: 'node-namespace', pollIntervalMs: 100000,
      mailbox: { encodedResultBytes: 8192, encodedSubmissionBytes: 4096, qualified: async () => true },
      async resolveAgent(claim) {
        const configFile = path.join(directory, 'profile.json');
        fs.writeFileSync(configFile, JSON.stringify({ scodeArgv: [process.execPath,
          agentFixture, 'mailbox'], workspace: claim.workspace,
          acpSessionId: 'ack-drain-session', closeTimeoutMs: 900,
          env: { SCODE_DIRECT_FIXTURE_ACTOR: claim.actor, HYDRA_DIRECT_FIXTURE_JOURNAL: journal,
            SCODE_MAILBOX_ACK_BARRIER: barrier } }), { flag: 'wx', mode: 0o600 });
        return { actor: claim.actor, configFile };
      },
    });
    const connection = owner.connectionFor(threadId), mailbox = owner.mailboxFor(threadId);
    const offer = await mailbox.inbox({});
    assert.match(offer.content[0].text, /⟦ack fixture-ack⟧/);
    let ackDone = false, unloadDone = false;
    ackTask = mailbox.inbox({ ack: ['fixture-ack'] }).then(result => ({ result }), error => ({ error }))
      .finally(() => { ackDone = true; });
    const deadline = performance.now()+5000;
    while (!rows(journal).some(row => row.method === 'ack-waiting') && performance.now()<deadline) await pause(10);
    assert(rows(journal).some(row => row.method === 'ack-waiting'), 'ACK must reach the real child before unload');
    loaded = false;
    unloadTask = owner.refresh().finally(() => { unloadDone = true; });
    await pause(200);
    assert.equal(ackDone, false, 'In-flight ACK must remain pending until its reply');
    assert.equal(unloadDone, false, 'Root removal must wait for ACK drain');
    assert.equal(connection.closed, false, 'ACP child must stay open while its ACK is in flight');
    fs.writeFileSync(barrier, '', { flag: 'wx', mode: 0o600 });
    const ack = await ackTask;
    assert.equal(ack.error, undefined, String(ack.error));
    assert.match(ack.result.content[0].text, /No unread mailbox frames/);
    await unloadTask;
    assert.equal(connection.closed, true);
    await connection.exited;
    assert.deepEqual(owner.status(), []);
    assert.equal(rows(journal).filter(row => row.method === 'peer_offer' && row.params.ack?.length).length, 1);
    assert.equal(rows(journal).filter(row => row.method === 'peer_offer_result')
      .flatMap(row => row.response.commits).filter(row => row.status === 'committed').length, 1);
    assert.equal(rows(journal).filter(row => row.method === 'session/load').length, 1);
    assert.equal(nativeFailure, undefined);
    console.log('PASS root unload drains the original ACP ACK before closing and joining its child; one commit, no reconnect.');
  } finally {
    if (!fs.existsSync(barrier)) fs.writeFileSync(barrier, '', { mode: 0o600 });
    await Promise.allSettled([ackTask, unloadTask]);
    await owner?.close();
    for (const socket of endpoint.clients) socket.terminate();
    await new Promise((resolve, reject) => endpoint.close(error => error ? reject(error) : resolve()));
    assert.equal(path.dirname(directory), path.resolve(os.tmpdir()));
    assert(path.basename(directory).startsWith('scode-ack-drain-'));
    fs.rmSync(directory, { recursive: true });
  }
}

main().catch(error => { console.error(error); process.exitCode = 1; });
