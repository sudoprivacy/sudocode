// Private paired-process fixture. Real scode owns append, offers, tokens and
// commits; the loopback model only copies tokens from its actual request.
const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
const crypto = require('node:crypto');
const assert = require('node:assert/strict');
const { once } = require('node:events');

exports.prepare = async function ({ root, executable, library, internals, env, receipt, restartBeforeAck = false, burst = false, truncation = false }) {
  const { connectScode, sendScodeMessage } = require(library);
  const { openCodexMailboxOwner, openScodeMailbox } = require(internals);
  const evidence = receipt.realMailbox = { scope: 'Real local-pair append/offer/ACK, not authenticated Nexus Node enrollment or release acceptance',
    offers: [], tools: [], unreadEvents: [], modelPresentations: [], children: [], cleanup: [] };
  const multipleFrames = burst || truncation;
  const bound = multipleFrames ? 32768 : 16384, actor = 'fixture-actor', peer = 'fixture-peer';
  const configHome = path.join(root, 'scode-config');
  fs.mkdirSync(configHome, { mode: 0o700 });
  let receiver, sender, owner, threadId, native;
  const openOwner = async () => {
    const current = receiver;
    const connection = { ...current, request: async (method, params) => {
      const row = { method, params, pid: current.pid, sessionId: current.sessionId }; evidence.offers.push(row);
      try { row.result = await current.request(method, params); return row.result; }
      catch (error) { row.error = String(error); throw error; }
    } };
    if (truncation) {
      // Deliberate over-budget presentation measurement. Do not start the
      // production wake controller or pretend this small native limit qualifies
      // the offer. An explicit private user turn reads the mailbox instead.
      const mailbox = openScodeMailbox(connection, threadId, bound);
      owner = { mailbox, close: () => mailbox.close() };
      return;
    }
    owner = await openCodexMailboxOwner({ actor, threadId, connection, native,
      callbackLog: path.join(root, 'real-mailbox.callback.jsonl'),
      // Measurement only. This does not enable runtime JSON admission.
      admission: { encodedResultBytes: bound, encodedSubmissionBytes: bound, qualified: async () => true } });
  };
  const restart = async () => {
    completeModelBody();
    const previous = receiver;
    const key = path.join(configHome, 'peer-input', crypto.createHash('sha256').update(actor).digest('hex')+'.ack-key');
    const keyHash = () => crypto.createHash('sha256').update(fs.readFileSync(key)).digest('hex');
    const record = evidence.restart = { beforePid: previous.pid, sessionId: previous.sessionId,
      beforeAckKeySha256: keyHash(), beforeTools: evidence.tools.length, startedAt: Date.now() };
    assert.equal(evidence.offers.filter(row => row.params?.ack?.length).length, 0);
    await owner.close();
    await previous.close();
    record.previousExit = await previous.exited;
    assert.deepEqual(record.previousExit, { code: 0, signal: null });
    receiver = await connect(actor, previous.sessionId);
    record.afterPid = receiver.pid;
    assert.notEqual(receiver.pid, previous.pid);
    assert.equal(receiver.sessionId, previous.sessionId, 'Restart must load the original ACP session');
    record.afterAckKeySha256 = keyHash();
    assert.equal(record.afterAckKeySha256, record.beforeAckKeySha256, 'Original token authority survives restart');
    await openOwner();
    const again = await owner.mailbox.inspect();
    assert.equal(again.unread, 1); assert.equal(again.frames.length, 1);
    assert.equal(again.frames[0].frameId, evidence.beforeAck.frame.frameId);
    assert.deepEqual(again.frames[0].deliveryIds, evidence.beforeAck.frame.deliveryIds);
    assert.equal(again.frames[0].redelivery, false, 'Fresh scode has no in-memory offered set');
    const repeated = await owner.mailbox.inspect();
    assert.equal(repeated.frames[0].redelivery, true);
    assert.equal(repeated.frames[0].frameId, again.frames[0].frameId);
    record.unreadBeforeOriginalAck = again.unread;
    record.finishedAt = Date.now();
  };
  const secret = crypto.randomBytes(32).toString('hex');
  const bridge = http.createServer(async (req, res) => {
    try {
      assert.equal(req.method, 'POST'); assert.equal(req.url, '/inbox');
      assert.equal(req.headers.authorization, 'Bearer '+secret);
      let bytes = 0; const chunks = [];
      for await (const chunk of req) { bytes += chunk.length; assert(bytes <= 16384); chunks.push(chunk); }
      const params = JSON.parse(Buffer.concat(chunks));
      assert(owner && threadId, 'Private binding is not admitted yet');
      assert.equal(params._meta?.threadId, threadId, 'Actual native MCP caller must match the bound user root');
      assert.equal(params.name, 'scode_inbox');
      if (restartBeforeAck && params.arguments?.ack?.length && !evidence.restart) {
        assert.equal(params.arguments.ack[0], ack, 'Restart only after the model selected its original token');
        await restart();
      }
      if (truncation) {
        const view = await owner.mailbox.inspect();
        evidence.truncation.unreadBeforeCalls.push({ args: params.arguments, unread: view.unread });
        assert.equal(view.unread, 5-evidence.truncation.presentedFrames.length
          + (params.arguments?.ack?.length && !evidence.truncation.duplicateIssued ? 1 : 0));
      }
      const result = await owner.mailbox.inbox(params.arguments);
      assert(Buffer.byteLength(JSON.stringify(result)) <= bound);
      evidence.tools.push({ params, result });
      res.writeHead(200, { 'content-type': 'application/json' }); res.end(JSON.stringify(result));
    } catch (error) {
      evidence.bridgeError ??= String(error.stack ?? error);
      res.writeHead(500); res.end('Private mailbox fixture refused');
    }
  });
  bridge.listen(0, '127.0.0.1'); await once(bridge, 'listening');
  fs.writeFileSync(path.join(root, 'mailbox-bridge.json'), JSON.stringify({ url: 'http://127.0.0.1:'+bridge.address().port+'/inbox', secret }),
    { mode: 0o600, flag: 'wx' });
  // Startup may query the provider's model catalog. Keep that discovery denied
  // locally too, but distinguish it from inference by method and exact route.
  // Every other request, including POST /v1/messages, fails verification.
  const denied = http.createServer((req, res) => {
    evidence.deniedInnerRequests ??= [];
    evidence.deniedInnerRequests.push({ method: req.method, path: req.url });
    res.writeHead(503); res.end();
  });
  denied.listen(0, '127.0.0.1'); await once(denied, 'listening');
  fs.writeFileSync(path.join(configHome, 'sudocode.json'), JSON.stringify({
    auth_modes: { 'api-key': { anthropic: { baseUrl: 'http://127.0.0.1:'+denied.address().port, apiKey: 'private-fixture-only' } } },
    models: { sonnet: { alias: 'sonnet', name: 'Private mailbox fixture', input: ['text'],
      providers: { 'api-key': { provider: 'anthropic', model: 'claude-sonnet-4-6' } } } },
  }), { mode: 0o600, flag: 'wx' });
  const scrubbed = Object.fromEntries(Object.keys(process.env).map(key => [key, null]));
  Object.assign(scrubbed, env, { SUDO_CODE_CONFIG_HOME: configHome, NO_COLOR: '1' });
  const connect = async (name, acpSessionId) => {
    const workspace = path.join(root, name);
    fs.mkdirSync(path.join(workspace, '.nexus/sudocode'), { recursive: true, mode: 0o700 });
    fs.writeFileSync(path.join(workspace, '.nexus/sudocode/settings.json'), JSON.stringify({ agentName: name }), { mode: 0o600 });
    const connection = await connectScode({ workspace, env: scrubbed, ...(acpSessionId ? { acpSessionId } : {}),
      scodeArgv: [executable, '--auth', 'api-key', '--model', 'sonnet', '--permission-mode', 'workspace-write', 'acp'],
      peerInputVersion: 3, onPeerUnread: event => { evidence.unreadEvents.push({ actor: name, ...event }); },
      stderrLog: path.join(root, name+'.stderr'), startupTimeoutMs: 15000, closeTimeoutMs: 6000 });
    evidence.children.push({ actor: name, pid: connection.pid, sessionId: connection.sessionId, initialized: connection.initialized });
    return connection;
  };
  let ack, burstTokens, duplicateIssued = false, calls = 0;
  const body = 'REAL-MESSAGE-BEGIN\n'+Array.from({ length: 180 }, (_, i) => `line ${i} 正文🙂 "quoted" \\ escaped\n`).join('')+'REAL-MESSAGE-END';
  const burstBodies = Array.from({ length: 5 }, (_, i) => `BURST-${i}-BEGIN 正文🙂 "quoted" \\ escaped BURST-${i}-END`);
  const completeModelBody = () => {
    if (burst) {
      const visible = evidence.modelPresentations.join('\n');
      for (const text of burstBodies) assert(visible.includes(text), 'Actual model receives each complete burst body');
      return visible;
    }
    // Windows split at UTF-8 boundaries, not necessarily line boundaries.
    // Strip only this fixture's framing and concatenate the original slices.
    const windows = evidence.modelPresentations.map(text =>
      /(?:^|\n)⟦message [^\n]+ · window (\d+)\/(\d+)⟧\n([\s\S]*?)\n⟦(?:next|ack) [^\s⟧]+⟧/.exec(text)).filter(Boolean);
    assert(windows.length > 1);
    const count = Number(windows[0][2]); assert.equal(windows.length, count);
    windows.forEach((entry, i) => { assert.equal(Number(entry[1]), i+1); assert.equal(Number(entry[2]), count); });
    const visible = windows.map(entry => entry[3]).join('');
    const start = visible.indexOf('REAL-MESSAGE-BEGIN'), end = visible.indexOf('REAL-MESSAGE-END');
    assert(start >= 0 && end > start);
    assert.equal(visible.slice(start, end+'REAL-MESSAGE-END'.length), body, 'Actual model receives the exact complete body across windows');
    return visible;
  };
  const close = async () => {
    const errors = [];
    for (const [label, item] of [['owner', owner], ['sender', sender], ['receiver', receiver]]) {
      if (!item) continue;
      try { await item.close(); if (item.exited) evidence.cleanup.push({ label, exit: await item.exited }); else evidence.cleanup.push({ label, joined: true }); }
      catch (error) { errors.push(error); evidence.cleanup.push({ label, error: String(error) }); }
    }
    for (const server of [bridge, denied]) { server.closeAllConnections(); await new Promise(resolve => server.close(resolve)); }
    fs.unlinkSync(path.join(root, 'mailbox-bridge.json'));
    if (errors.length) throw new AggregateError(errors, 'Private scode cleanup failed');
  };
  try { receiver = await connect(actor); sender = await connect(peer); }
  catch (error) { await close(); throw error; }
  return {
    bound,
    async bind(nativeConnection, id) {
      threadId = id; native = nativeConnection;
      await openOwner();
      if (multipleFrames) {
        const started = performance.now();
        evidence.burst = { calls: [], results: [],
          bodySha256: burstBodies.map(text => crypto.createHash('sha256').update(text).digest('hex')) };
        // One session has one direct-tool lane. Keep the burst's one-second
        // bound while awaiting each admission; never retry a refused send.
        for (const [index, message] of burstBodies.entries()) {
          const call = { index, startedMs: performance.now()-started };
          evidence.burst.calls.push(call);
          try {
            const result = await sendScodeMessage(sender, { to: actor, message, summary: 'Private native mailbox burst measurement' });
            call.result = result; evidence.burst.results.push(result);
            assert.equal(result.isError, false, result.content);
          } catch (error) {
            call.error = { message: String(error), outcome: error.outcome, rpcError: error.rpcError };
            throw error;
          } finally { call.finishedMs = performance.now()-started; }
        }
        evidence.burst.elapsedMs = performance.now()-started;
        if (burst) assert(evidence.burst.elapsedMs <= 1000, 'All five real appends must complete within one second');
      } else {
        evidence.send = await sendScodeMessage(sender, { to: actor, message: body, summary: 'Private native mailbox ACK measurement' });
        assert.equal(evidence.send.isError, false, evidence.send.content);
      }
      const deadline = Date.now()+10000;
      let first;
      do { first = await owner.mailbox.inspect(); if (first.unread) break; await new Promise(resolve => setTimeout(resolve, 50)); } while (Date.now()<deadline);
      assert.equal(first.unread, multipleFrames ? 5 : 1); assert.equal(first.frames.length, multipleFrames ? 5 : 1);
      if (multipleFrames) {
        assert(first.frames.every(frame => frame.complete));
        assert.equal(new Set(first.frames.map(frame => frame.frameId)).size, 5);
        assert.equal(new Set(first.frames.flatMap(frame => frame.deliveryIds)).size, 5);
        assert.equal([...first.text.matchAll(/⟦ack ([^\s⟧]+)⟧/g)].length, 5);
        evidence.burst.frames = first.frames;
      } else {
        assert(first.frames[0].window?.count > 1 && !first.frames[0].complete);
        assert(!first.text.includes('⟦ack '), 'First window cannot expose a completion ACK');
      }
      const again = await owner.mailbox.inspect();
      assert.equal(again.unread, multipleFrames ? 5 : 1);
      assert.deepEqual(again.frames.map(frame => frame.deliveryIds), first.frames.map(frame => frame.deliveryIds));
      assert(again.frames.every(frame => frame.redelivery));
      evidence.beforeAck = multipleFrames ? { unread: again.unread, frames: again.frames }
        : { unread: again.unread, frame: again.frames[0], bodySha256: crypto.createHash('sha256').update(body).digest('hex') };
      if (truncation) {
        evidence.truncation = { nativeOutputTokens: 256, presentedFrames: [], unreadBeforeCalls: [],
          modelOutputs: [], truncatedOffers: 0, duplicateIssued: false };
        return;
      }
      return { evaluate: () => owner.evaluate(), status: () => owner.status(), close: () => owner.close() };
    },
    nextTruncationCall(modelInput, previousCallId) {
      if (evidence.bridgeError) throw new Error(evidence.bridgeError);
      assert(++calls <= 14, 'Private truncation recovery call bound');
      if (!evidence.tools.length) return {};
      const outputs = modelInput.filter(item => item.type === 'function_call_output' && item.call_id === previousCallId);
      assert.equal(outputs.length, 1, 'The model must see the most recent actual tool response');
      const text = typeof outputs[0].output === 'string' ? outputs[0].output
        : outputs[0].output.filter(item => item.type === 'input_text').map(item => item.text).join('\n');
      const observed = evidence.truncation;
      const last = evidence.tools.at(-1);
      observed.modelOutputs.push({ callId: previousCallId, text });
      if (last.params.arguments.read) {
        assert(text.includes(last.result.content[0].text), 'Read-alone result must reach the actual model byte-for-byte');
        assert(!/tokens truncated/.test(text), 'Read-alone response must fit the unchanged native limit');
        const frame = evidence.beforeAck.frames.find(frame => frame.frameId === last.params.arguments.read);
        assert(frame && !observed.presentedFrames.includes(frame.frameId));
        const index = evidence.beforeAck.frames.indexOf(frame);
        assert(text.includes(burstBodies[index]), 'Model must receive the complete message before its ACK');
        const tokens = [...text.matchAll(/⟦ack ([^\s⟧]+)⟧/g)].map(match => match[1]);
        assert.equal(tokens.length, 1);
        observed.presentedFrames.push(frame.frameId);
        ack = tokens[0];
        return { ack: [ack] };
      }
      if (observed.modelOutputs.length === 1) {
        assert(/tokens truncated/.test(text), 'Native must actually truncate the combined offer');
        assert(text.includes('BURST-0-BEGIN') && text.includes('BURST-4-END'));
        assert(!text.includes('BURST-2-BEGIN'), 'The middle frame must be missing from the model request');
      }
      if (/tokens truncated/.test(text)) observed.truncatedOffers++;
      const remaining = /⟦unread (\d+)⟧/.exec(text);
      assert(remaining, 'Actual model retains the unread count');
      if (Number(remaining[1])) {
        const frame = /⟦message [^\n]*? · frame ([^\s]+) ·/.exec(text);
        assert(frame, 'Read-alone identity must come from the actual model request');
        return { read: frame[1] };
      }
      assert.equal(observed.presentedFrames.length, 5);
      if (!observed.duplicateIssued) { observed.duplicateIssued = true; return { ack: [ack] }; }
      return undefined;
    },
    async verifyTruncation() {
      const observed = evidence.truncation;
      assert(observed.truncatedOffers > 0 && observed.duplicateIssued);
      assert.equal(observed.presentedFrames.length, 5);
      assert.equal(new Set(observed.presentedFrames).size, 5);
      const view = await owner.mailbox.inspect(); assert.equal(view.unread, 0); assert.deepEqual(view.frames, []);
      const acknowledgements = evidence.offers.filter(row => row.params?.ack?.length);
      assert.equal(acknowledgements.length, 6);
      for (const [index, row] of acknowledgements.entries()) {
        if (index === 5) { assert.deepEqual(row.result.commits, []); continue; }
        const frame = evidence.beforeAck.frames[index];
        const range = /^:(\d+)-(\d+)$/.exec(frame.frameId.slice(frame.conversation.length));
        assert(range);
        assert.deepEqual(row.result.commits, [{ conversation: frame.conversation,
          from: Number(range[1]), to: Number(range[2]), status: 'committed' }]);
        assert.equal(row.result.unread, 4-index);
      }
      const requests = evidence.deniedInnerRequests ?? [];
      assert(requests.every(request => request.method === 'GET' && request.path === '/v1/models'));
      observed.completeReadBeforeEveryAck = true;
      observed.distinctFramesCommitted = 5;
      evidence.innerInferenceRequests = 0; evidence.deniedCatalogRequests = requests.length;
      evidence.finalUnread = view.unread;
    },
    nextCall(inputs, delivery) {
      if (evidence.bridgeError) throw new Error(evidence.bridgeError);
      const last = evidence.tools.at(-1)?.result.content[0].text;
      const text = last ?? delivery;
      assert(inputs.some(value => value.includes(text)), 'Actual model request must contain the entire most recent scode presentation');
      if (!evidence.modelPresentations.includes(text)) evidence.modelPresentations.push(text);
      assert(++calls <= 20, 'Private model call bound');
      const continuation = /⟦continue with scode_inbox\(\{read: "([^"]+)", window: (\d+), after: "([^"]+)"\}\)⟧/.exec(text);
      if (continuation) {
        assert(!text.includes('⟦ack '), 'An incomplete window must have no ACK');
        return { read: continuation[1], window: Number(continuation[2]), after: continuation[3] };
      }
      // Diagnostics also begin with "ack"; only a single opaque token is an
      // acknowledgement instruction, never the "ack not recognised" prose.
      const tokens = [...text.matchAll(/⟦ack ([^\s⟧]+)⟧/g)].map(match => match[1]);
      if (tokens.length) {
        assert.equal(tokens.length, burst ? 5 : 1); completeModelBody(); ack = tokens[0];
        if (burst) { assert.equal(new Set(tokens).size, 5); burstTokens = tokens; }
        return { ack: [...(burstTokens ?? [ack]), 'not-a-real-ack'] };
      }
      if (ack && !duplicateIssued) { duplicateIssued = true; return { ack: burstTokens ?? [ack] }; }
      assert(duplicateIssued && text.includes('⟦unread 0⟧'));
      return undefined;
    },
    async verify() {
      const view = await owner.mailbox.inspect(); assert.equal(view.unread, 0); assert.deepEqual(view.frames, []);
      const ackRequests = evidence.offers.filter(row => row.params?.ack?.length);
      assert.equal(ackRequests.length, 2, 'Only actual model ACK calls commit');
      assert.equal(ackRequests[0].result.commits.length, 1);
      assert.equal(ackRequests[1].result.commits.length, 0, 'Duplicate model ACK cannot commit twice');
      assert.equal(ackRequests[0].result.unread, 0);
      assert.match(ackRequests[0].result.text, /ack not recognised/);
      if (restartBeforeAck) {
        assert(evidence.restart?.finishedAt, 'Real receiver must restart before the original model ACK');
        assert.equal(ackRequests[0].pid, evidence.restart.afterPid);
        assert.equal(ackRequests[0].params.ack[0], ack);
        assert.equal(evidence.children.filter(child => child.actor === actor).length, 2);
        evidence.restart.originalTokenCommittedOnce = true;
      }
      assert(evidence.modelPresentations.at(-1).includes('⟦unread 0⟧'));
      const visible = completeModelBody();
      if (burst) {
        assert.equal(ackRequests[0].params.ack.length, 6);
        assert.equal(ackRequests[1].params.ack.length, 5);
        const frames = evidence.burst.frames;
        const ranges = frames.map(frame => {
          const range = /^:(\d+)-(\d+)$/.exec(frame.frameId.slice(frame.conversation.length));
          assert(range); return range.slice(1).map(Number);
        }).sort((a, b) => a[0]-b[0]);
        assert(frames.every(frame => frame.conversation === frames[0].conversation));
        for (let i=1; i<ranges.length; i++) assert.equal(ranges[i-1][1], ranges[i][0]);
        assert.deepEqual(ackRequests[0].result.commits[0], { conversation: frames[0].conversation,
          from: ranges[0][0], to: ranges.at(-1)[1], status: 'committed' });
        evidence.burst.allBodiesPresentedBeforeAck = true;
        evidence.burst.distinctFramesCommitted = frames.length;
      } else {
        assert(visible.includes('REAL-MESSAGE-BEGIN') && visible.includes('REAL-MESSAGE-END'));
        for (let i=0; i<180; i++) assert(visible.includes(`line ${i} 正文`), 'Actual model receives line '+i);
      }
      const requests = evidence.deniedInnerRequests ?? [];
      assert(requests.every(request => request.method === 'GET' && request.path === '/v1/models'),
        'No inner scode inference or other provider request');
      evidence.deniedCatalogRequests = requests.length;
      evidence.innerInferenceRequests = 0;
      if (!burst) evidence.committedOnceAfterAllWindows = true;
      evidence.finalUnread = view.unread;
    },
    close,
  };
};
