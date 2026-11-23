// Actual native server + actual TUI + model-facing Responses capture.
// This is a bounded presentation/correlation qualification, not a live move,
// full v3 admission or bilateral-idle acceptance. real-mailbox additionally
// uses two real scode processes on an isolated local-pair namespace.
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
const http = require('node:http');
const crypto = require('node:crypto');
const assert = require('node:assert/strict');
const { spawn, execFileSync } = require('node:child_process');
const { once } = require('node:events');
const { parseArgs } = require('node:util');
const { performance } = require('node:perf_hooks');
const { socketEntryPresent } = require('./fixtures/nativeSocket.cjs');
const options = parseArgs({ options: { codex: { type: 'string' }, scode: { type: 'string' }, output: { type: 'string' },
  driver: { type: 'string' }, 'process-probe': { type: 'string' }, 'temp-root': { type: 'string' },
  'previous-scode': { type: 'string' },
  case: { type: 'string', default: 'idle' } } }).values;
assert(['idle', 'active-reply-loss', 'interrupted', 'request-loss-budget', 'owner-crash-budget', 'whole-budget', 'real-mailbox', 'real-mailbox-restart', 'real-mailbox-burst', 'real-mailbox-truncation', 'native-settings-probe', 'native-settings-warm-resume', 'native-settings-cold-resume'].includes(options.case));
const realMailboxCase = ['real-mailbox', 'real-mailbox-restart', 'real-mailbox-burst', 'real-mailbox-truncation'].includes(options.case);
const realMailboxBurst = options.case === 'real-mailbox-burst';
const realMailboxTruncation = options.case === 'real-mailbox-truncation';
const coldSettingsResume = options.case === 'native-settings-cold-resume';
const settingsResume = coldSettingsResume || options.case === 'native-settings-warm-resume';
const settingsProbe = settingsResume || options.case === 'native-settings-probe';
if (realMailboxCase) assert(options.scode && path.isAbsolute(options.scode) && fs.statSync(options.scode).isFile());
const previousScode = options['previous-scode'];
if (previousScode !== undefined) {
  assert.equal(options.case, 'real-mailbox-restart', 'An existing-session upgrade uses the restart journey');
  assert(path.isAbsolute(previousScode) && fs.statSync(previousScode).isFile());
}
const encodedBound = realMailboxBurst || realMailboxTruncation ? 32768 : realMailboxCase || settingsProbe ? 16384 : 4096;
const activeReplyLoss = options.case === 'active-reply-loss';
const interrupted = options.case === 'interrupted';
const requestLoss = options.case === 'request-loss-budget';
const ownerCrash = options.case === 'owner-crash-budget';
const unknownSubmission = requestLoss || ownerCrash;
const wholeBudget = options.case === 'whole-budget';
assert(options.codex && path.isAbsolute(options.codex) && fs.statSync(options.codex).isFile());
assert(options.output && path.isAbsolute(options.output));
assert(['darwin', 'win32'].includes(process.platform), 'This real PTY fixture supports macOS and Windows');
const driverArgv = options.driver ? JSON.parse(options.driver) : process.platform === 'win32' ? null
  : ['python3', path.join(__dirname, 'fixtures/nativeCodexTui.py')];
assert(Array.isArray(driverArgv) && driverArgv.length && driverArgv.every(arg => typeof arg === 'string' && arg && !arg.includes('\0')),
  'A real TUI driver is required; Windows must provide --driver JSON argv. No protocol-front-end fallback.');
const probeArgv = options['process-probe'] ? JSON.parse(options['process-probe']) : [];
assert(Array.isArray(probeArgv) && probeArgv.every(arg => typeof arg === 'string' && arg && !arg.includes('\0'))
  && (process.platform !== 'win32' || probeArgv.length), 'Windows requires --process-probe JSON argv for the native lookup helper');
const sha = file => crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
if (previousScode) assert.notEqual(sha(previousScode), sha(options.scode), 'Upgrade requires two distinct binaries');
const output = options.output;
fs.mkdirSync(output, { recursive: true, mode: 0o700 });
assert(!fs.existsSync(path.join(output, 'receipt.json')), 'Preserve each original case result');
const root = fs.mkdtempSync(path.join(options['temp-root'] ?? os.tmpdir(), 'cx-mail-'));
fs.chmodSync(root, 0o700);
for (const name of ['home', 'user', 'work']) fs.mkdirSync(path.join(root, name), { mode: 0o700 });
const home = path.join(root, 'home'), work = path.join(root, 'work');
const socket = path.join(home, 'app-server-control/app-server-control.sock');
const driverFiles = driverArgv.filter(file => fs.existsSync(file) && fs.statSync(file).isFile());
const probeFiles = probeArgv.filter(file => fs.existsSync(file) && fs.statSync(file).isFile());
const promptGuard = path.join(__dirname, 'fixtures/nativeTuiPrompt.py');
const tool = path.join(__dirname, realMailboxCase ? 'fixtures/nativeRealMailboxTool.cjs' : 'fixtures/nativeMailboxTool.cjs');
const realFixture = path.join(__dirname, 'fixtures/realScodeMailbox.cjs');
const ownerFixture = path.join(__dirname, 'fixtures/nativeMailboxOwner.cjs');
const library = path.resolve(__dirname, '../dist/index.cjs');
const internals = path.resolve(__dirname, '../dist/testing/nativeMailboxInternals.cjs');
const manifest = JSON.parse(fs.readFileSync(path.resolve(__dirname, '../manifest.json')));
assert.equal(manifest.files['dist/index.cjs'].sha256, sha(library));
const { connectCodex, CodexRequestError, CodexMailboxPresentation, runCodexMcpOwner, shutdownCommand } = require(library);
const { openCodexMailboxWake, readCodexMailboxSnapshot, mailboxSubmissionTextBudget,
  CODEX_MAILBOX_DELIVERY_PROVENANCE } = require(internals);
const env = {};
for (const key of ['PATH', 'SystemRoot', 'WINDIR', 'COMSPEC', 'PATHEXT', 'TMPDIR', 'TEMP', 'TMP', 'LANG', 'LC_ALL', 'SHELL'])
  if (process.env[key] !== undefined) env[key] = process.env[key];
Object.assign(env, { CODEX_HOME: home, HOME: path.join(root, 'user'), USERPROFILE: path.join(root, 'user'),
  APPDATA: path.join(root, 'roaming'), LOCALAPPDATA: path.join(root, 'local'),
  XDG_CONFIG_HOME: path.join(root, 'xdg'), NPM_CONFIG_PREFIX: path.join(root, 'npm'),
  TERM: 'xterm-256color', NO_PROXY: '127.0.0.1,localhost,::1', no_proxy: '127.0.0.1,localhost,::1',
  NATIVE_MAILBOX_FIXTURE_KEY: 'isolated-fixture-only' });
const receipt = { at: new Date().toISOString(), status: 'RUNNING', root,
  case: options.case, platform: process.platform, driverArgv, probeArgv,
  scope: 'Actual installed native, isolated server/TUI, deterministic loopback model. Native presentation and clientId correlation only.',
  liveConfigChanged: false, liveEndpointUsed: false, fullMailboxQualified: false,
  sourceCommit: manifest.sourceCommit, buildDirty: manifest.dirty,
  inputs: Object.fromEntries([options.codex, __filename, ...driverFiles, ...probeFiles, promptGuard, tool, library, internals,
    ...(realMailboxCase ? [options.scode, realFixture] : []), ...(previousScode ? [previousScode] : []),
    ...(ownerCrash ? [ownerFixture] : [])].map(file => [file, sha(file)])),
  requests: [], fixtureActions: [], notifications: [], modelRequests: [], stages: [], cleanup: [] };
const save = () => fs.writeFileSync(path.join(output, 'receipt.json'), JSON.stringify(receipt, null, 2)+'\n', { mode: 0o600 });
const bounded = async (promise, ms, label) => {
  let timer;
  try { return await Promise.race([promise, new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(label+' timeout')), ms); })]); }
  finally { clearTimeout(timer); }
};
const wait = async (predicate, ms, label) => {
  const deadline = Date.now()+ms;
  do { if (await predicate()) return; await new Promise(resolve => setTimeout(resolve, 50)); } while (Date.now()<deadline);
  throw new Error(label+' timeout');
};
const stage = name => { receipt.stages.push(name); save(); console.log(name); };
const fillBudget = (prefix, suffix, bytes) => {
  const content = prefix+'\n正文🙂"\\\t\n';
  return content+'\u0001'.repeat(bytes-Buffer.byteLength(content+suffix))+suffix;
};
let phase = 'seed', expectedDelivery, expectedWindow, callIssued = false, server, serverExit, tui, tuiExit, client, wake, realMailbox;
let sdkOwner, sdkStartupError;
const sdkRuntime = path.join(root, 'sdk-runtime.json');
const sdkEntry = path.join(path.dirname(library), 'server.cjs');
const sdkRegistration = (id, limit) => ({ command: process.execPath, required: true,
  args: [sdkEntry, '--codex', '--config', sdkRuntime, '--registration-id', id],
  tools: { scode_inbox: { approval_mode: 'approve', output_token_limit: limit } } });
const sdkMeasurement = settingsResume ? new CodexMailboxPresentation() : undefined;
let sdkSample, sdkCallIssued = false;
let realToolSequence = 0, realModelCompleted = false;
let nativeStderr = '', tuiStderr = '', tuiBuffer = '';
const tuiEvents = [];
let endpointFailure;
let releaseHuman, humanTurn;
const humanText = 'Isolated active user turn '+crypto.randomUUID();
const truncationPrompt = 'Read the isolated mailbox; read any truncated message alone before acknowledging it.';
const strings = value => typeof value === 'string' ? [value] : Array.isArray(value) ? value.flatMap(strings)
  : value && typeof value === 'object' ? Object.values(value).flatMap(strings) : [];
const endpoint = http.createServer((req, res) => {
  const chunks = [], observation = { method: req.method, url: req.url, phase, remotePort: req.socket.remotePort };
  receipt.modelRequests.push(observation);
  let bytes = 0;
  req.on('data', chunk => { bytes += chunk.length; if (bytes > 2*1024*1024) req.destroy(new Error('Fixture model request too large')); else chunks.push(chunk); });
  req.on('end', () => {
    try {
      assert.equal(req.method, 'POST'); assert.equal(req.url, '/v1/responses');
      const body = Buffer.concat(chunks), value = JSON.parse(body);
      const file = path.join(output, 'model-'+receipt.modelRequests.indexOf(observation)+'.json');
      fs.writeFileSync(file, body, { mode: 0o600, flag: 'wx' });
      observation.file = file; observation.sha256 = sha(file); observation.bytes = body.length;
      if (process.platform === 'win32') {
        // Query the original connection tuple through the Windows IP Helper
        // API, without PowerShell/CIM startup inside the response deadline.
        assert(Number.isInteger(observation.remotePort) && Number.isInteger(endpoint.address().port));
        observation.nativePid = JSON.parse(execFileSync(probeArgv[0], [...probeArgv.slice(1), 'tcp',
          String(observation.remotePort), String(endpoint.address().port)], { env, encoding: 'utf8', timeout: 3000 }));
      } else {
        const sockets = execFileSync('/usr/sbin/lsof', ['-nP', '-iTCP:'+endpoint.address().port, '-sTCP:ESTABLISHED', '-Fpn'], { encoding: 'utf8', timeout: 3000 });
        let pid;
        for (const line of sockets.split('\n')) {
          if (line.startsWith('p')) pid = Number(line.slice(1));
          if (line === 'n127.0.0.1:'+observation.remotePort+'->127.0.0.1:'+endpoint.address().port) observation.nativePid = pid;
        }
      }
      assert.equal(observation.nativePid, server.pid, 'Model request must originate from the owned native server');
      const inputs = strings(value.input);
      if (realMailboxTruncation) {
        // Native title generation has its own thread and no MCP tools. Only
        // the explicit user turn may drive this mailbox conversation.
        observation.nativeThreadId = value.client_metadata?.thread_id;
        observation.truncationUserTurn = observation.nativeThreadId === receipt.rootThread?.id
          && value.input.some(item => item.role === 'user' && item.content?.some(part => part.text === truncationPrompt));
      }
      observation.exactDeliverySeen = expectedDelivery !== undefined && inputs.includes(expectedDelivery);
      observation.exactWindowSeen = expectedWindow !== undefined && inputs.some(text => text.includes(expectedWindow));
      if (settingsProbe) {
        observation.windowPresentations = inputs.filter(text => text.includes('SETTINGS-WINDOW-BEGIN') || text.includes('SETTINGS-WINDOW-END'));
        observation.truncatedWindowSeen = observation.windowPresentations.some(text =>
          text.includes('SETTINGS-WINDOW-BEGIN') && text.includes('SETTINGS-WINDOW-END')
          && /truncated/i.test(text) && !text.includes('SETTINGS-WINDOW-MIDDLE'));
      }
      const sampleOutput = value.input.find(item => item.type === 'function_call_output' && item.call_id === 'call_sdk_presentation');
      if (sdkSample && sampleOutput && value.client_metadata?.thread_id === sdkSample.threadId) {
        const parts = strings(sampleOutput.output);
        observation.sdkSampleExact = parts.some(part => part.includes(sdkSample.expectedText));
        observation.sdkSampleTruncated = parts.some(part => part.includes(sdkSample.expectedText.slice(0, 16))
          && part.includes(sdkSample.expectedText.slice(-16)) && /truncated/i.test(part)
          && !part.includes(sdkSample.expectedText.slice(1950, 1982)));
      }
      const id = 'resp_fixture_'+receipt.modelRequests.indexOf(observation);
      const events = [{ type: 'response.created', response: { id } }];
      if (phase === 'truncation' && observation.truncationUserTurn
        || phase === 'delivery' && observation.exactDeliverySeen && (!callIssued || realMailboxCase)) {
        const namespaced = (value.tools ?? []).find(entry => entry.type === 'namespace' && entry.name === 'mcp__mailbox_probe');
        const named = namespaced?.tools?.find(entry => entry.name === 'scode_inbox')
          ?? (value.tools ?? []).find(entry => entry.name === 'mcp__mailbox_probe__scode_inbox');
        assert(named, 'Native model request must advertise the actual fixture MCP tool');
        callIssued = true;
        const args = realMailboxTruncation ? realMailbox.nextTruncationCall(value.input, 'call_fixture_window_'+realToolSequence)
          : realMailboxCase ? realMailbox.nextCall(inputs, expectedDelivery) : { read: 'fixture-window-1' };
        if (args) {
          const suffix = realMailboxCase ? '_'+(++realToolSequence) : '';
          events.push({ type: 'response.output_item.done', item: { type: 'function_call', id: 'fc_fixture_window'+suffix,
            call_id: 'call_fixture_window'+suffix, ...(namespaced ? { namespace: namespaced.name } : {}),
            name: named.name, arguments: JSON.stringify(args) } });
        } else {
          realModelCompleted = true;
          events.push({ type: 'response.output_item.done', item: { type: 'message', role: 'assistant', id: 'msg_'+id,
            content: [{ type: 'output_text', text: 'Real scode mailbox handled and acknowledged.' }] } });
        }
      } else if (sdkSample && !sdkCallIssued && observation.exactDeliverySeen && phase === 'delivery'
          && value.client_metadata?.thread_id === sdkSample.threadId
          && value.input.some(item => item.type === 'function_call_output' && item.call_id === 'call_fixture_window')) {
        const namespace = value.tools.find(entry => entry.type === 'namespace' && entry.name === 'mcp__nexus_scode');
        const tool = namespace?.tools.find(entry => entry.name === 'scode_inbox')
          ?? value.tools.find(entry => entry.name === 'mcp__nexus_scode__scode_inbox');
        assert(tool && expectedDelivery.includes(sdkSample.read), 'The native model must receive the prepared SDK read');
        sdkCallIssued = true;
        events.push({ type: 'response.output_item.done', item: { type: 'function_call', id: 'fc_sdk_presentation',
          call_id: 'call_sdk_presentation', ...(namespace ? { namespace: namespace.name } : {}),
          name: tool.name, arguments: JSON.stringify({ read: sdkSample.read }) } });
      } else {
        events.push({ type: 'response.output_item.done', item: { type: 'message', role: 'assistant', id: 'msg_'+id,
          content: [{ type: 'output_text', text: 'Isolated native mailbox presentation completed.' }] } });
      }
      events.push({ type: 'response.completed', response: { id, usage: { input_tokens: 0, input_tokens_details: null,
        output_tokens: 0, output_tokens_details: null, total_tokens: 0 } } });
      const send = () => {
        res.writeHead(200, { 'content-type': 'text/event-stream' });
        for (const event of events) res.write('event: '+event.type+'\ndata: '+JSON.stringify(event)+'\n\n');
        res.end();
      };
      if (phase === 'active' && inputs.includes(humanText)) {
        assert(!releaseHuman, 'Only one user turn is held');
        observation.heldHumanTurn = true;
        releaseHuman = () => { observation.releasedAt = new Date().toISOString(); send(); };
      } else send();
    } catch (error) { endpointFailure ??= String(error.stack ?? error); res.writeHead(500); res.end('Private fixture failed'); }
  });
});
(async () => {
  try {
    receipt.nativeVersion = execFileSync(options.codex, ['--version'], { env, encoding: 'utf8', timeout: 10000 }).trim();
    endpoint.listen(0, '127.0.0.1'); await once(endpoint, 'listening');
    if (realMailboxCase) {
      receipt.scope = 'Actual native server/TUI with real scode local-pair append/window/ACK. Private loopback model; not Node enrollment, full native admission or live migration.';
      realMailbox = await require(realFixture).prepare({ root, executable: options.scode, previousScode, library, internals, env, receipt,
        restartBeforeAck: options.case === 'real-mailbox-restart', burst: realMailboxBurst, truncation: realMailboxTruncation });
      stage('real-scode-pair-connected');
    }
    // Deliberately includes escaping, multibyte text and sentinels at both ends.
    expectedWindow = 'WINDOW-BEGIN-'+crypto.randomUUID()+'\n'+('窗🙂"\\\t\n'.repeat(50))+'WINDOW-END';
    // The actual native thread ID below must have this measured UUID width.
    // This fills the same conservative text budget the composed owner uses,
    // with six-byte JSON escapes rather than a small ASCII smoke payload.
    const initialBudget = mailboxSubmissionTextBudget('fixture-actor', '0'.repeat(36), 4096);
    if (wholeBudget) expectedWindow = fillBudget('WINDOW-BEGIN', 'WINDOW-END', initialBudget);
    if (settingsProbe) expectedWindow = 'SETTINGS-WINDOW-BEGIN\n'+'x'.repeat(2048)
      +'\nSETTINGS-WINDOW-MIDDLE\n'+'y'.repeat(2048)+'\nSETTINGS-WINDOW-END';
    fs.writeFileSync(path.join(root, 'tool-fixture.json'), JSON.stringify({ cursor: 'fixture-window-1', text: expectedWindow,
      encodedResultBytes: encodedBound }), { mode: 0o600, flag: 'wx' });
    fs.writeFileSync(path.join(home, 'config.toml'),
      'check_for_update_on_startup = false\nmodel = "gpt-5.4"\nmodel_provider = "mailbox_fixture"\n'+
      'approval_policy = "never"\nsandbox_mode = "read-only"\n[features]\napps = false\nplugins = false\n'+
      '[analytics]\nenabled = false\n[model_providers.mailbox_fixture]\nname = "Isolated mailbox fixture"\n'+
      'base_url = "http://127.0.0.1:'+endpoint.address().port+'/v1"\nwire_api = "responses"\nenv_key = "NATIVE_MAILBOX_FIXTURE_KEY"\n'+
      '[projects.'+JSON.stringify(work)+']\ntrust_level = "trusted"\n'+
      '[mcp_servers.mailbox_probe]\ncommand = '+JSON.stringify(process.execPath)+'\nargs = ['+JSON.stringify(tool)+']\nrequired = true\n'+
      // Explicit permission for this fixture's sole read tool; never changes
      // global approval policy or a user's MCP server permissions.
      '[mcp_servers.mailbox_probe.tools.scode_inbox]\napproval_mode = "approve"\n'+
      (settingsProbe ? 'output_token_limit = 4096\n' : realMailboxTruncation ? 'output_token_limit = 256\n' : '')+
      '[mcp_servers.mailbox_probe.env]\nNATIVE_MAILBOX_FIXTURE_ROOT = '+JSON.stringify(root)+'\n', { mode: 0o600, flag: 'wx' });
    receipt.privateConfig = { sha256: sha(path.join(home, 'config.toml')),
      soleFixtureToolApproval: 'approve', approvalPolicy: 'never', sandboxMode: 'read-only' };
    server = spawn(options.codex, ['app-server', '--listen', 'unix://'], { cwd: work, env, stdio: ['ignore', 'ignore', 'pipe'] });
    serverExit = once(server, 'close'); server.stderr.on('data', data => { nativeStderr = (nativeStderr+data).slice(-20000); });
    receipt.nativePid = server.pid;
    await wait(() => socketEntryPresent(socket), 10000, 'Private native listener');
    client = await connectCodex({ codexArgv: [options.codex], socket, env }, event => {
      receipt.notifications.push(event); if (receipt.notifications.length > 10000) throw new Error('Fixture notification bound exceeded');
    });
    receipt.initialize = client.initialized; stage('actual-native-connected');
    const request = async (method, params) => {
      const row = { method, params }; receipt.requests.push(row);
      try {
        if (realMailboxCase && method === 'thread/queue/add') expectedDelivery = params.input[0].text;
        if (requestLoss && method === 'thread/queue/add'
            && receipt.requests.filter(row => row.method === 'thread/queue/add').length === 1) {
          row.simulatedRequestLoss = true;
          throw new CodexRequestError('unknown', 'Fixture did not forward the first queue-add request');
        }
        row.result = await client.request(method, params);
        if (activeReplyLoss && method === 'thread/queue/add') {
          // The real native accepts the request. Only its reply to the owner is
          // hidden; the unchanged owner must reconcile by native client ID.
          row.simulatedReplyLoss = true;
          throw new CodexRequestError('unknown', 'Fixture discarded accepted queue-add reply');
        }
        return row.result;
      }
      catch (error) { row.error = String(error); throw error; }
    };
    let settingsThread;
    let expectedGeneration;
    if (settingsProbe) {
      const params = { cwd: work, threadSource: 'user',
        config: { 'mcp_servers.mailbox_probe.tools.scode_inbox.output_token_limit': 64 } };
      if (settingsResume) {
        assert.equal(manifest.files['dist/server.cjs'].sha256, sha(sdkEntry));
        receipt.inputs[sdkEntry] = sha(sdkEntry);
        const listener = require('node:net').createServer();
        listener.listen(0, '127.0.0.1'); await once(listener, 'listening');
        const port = listener.address().port;
        await new Promise(resolve => listener.close(resolve));
        const auditLog = path.join(root, 'sdk-owner.jsonl');
        fs.writeFileSync(sdkRuntime, JSON.stringify({ version: 1, mode: 'codex-autojoin',
          native: { codexArgv: [options.codex], socket, env }, auditLog, stateDirectory: path.join(root, 'sdk-state'),
          peers: [], peerPolicy: 'node-namespace', endpoint: { port, token: crypto.randomBytes(32).toString('hex') },
          enrollmentArgv: [process.execPath, '-e', 'process.exit(1)'] }), {flag:'wx',mode:0o600});
        // The actual inbox can present one operator-prepared sample even while
        // the private owner refuses to attach an ACP receiver.
        sdkOwner = runCodexMcpOwner(sdkRuntime, { encodedResultBytes: 4096, encodedSubmissionBytes: 4096,
          qualified: async () => false }, sdkMeasurement);
        sdkOwner.catch(error => { sdkStartupError = error; });
        await wait(() => {
          if (sdkStartupError) throw sdkStartupError;
          return fs.existsSync(auditLog) && fs.readFileSync(auditLog, 'utf8').includes('"phase":"native_inventory"');
        }, 5000, 'Packaged SDK owner observes its private native server');
        expectedGeneration = crypto.randomUUID();
        params.config['mcp_servers.mailbox_probe.env.NATIVE_MAILBOX_FIXTURE_GENERATION'] = expectedGeneration;
        params.config['mcp_servers.nexus_scode'] = sdkRegistration(expectedGeneration, 64);
        if (coldSettingsResume) params.config.thread_unload_delay_secs = 0;
      }
      const started = await client.request('thread/start', params);
      settingsThread = started.thread.id;
      receipt.fixtureActions.push({ method: 'thread/start', params, result: started });
      const seedParams = { threadId: settingsThread, input: [{ type: 'text', text: 'Reply with the isolated fixture seed.' }] };
      const seed = await client.request('turn/start', seedParams);
      receipt.fixtureActions.push({ method: 'turn/start', params: seedParams, result: seed });
      await wait(async () => {
        if (endpointFailure) throw new Error(endpointFailure);
        if (!receipt.notifications.some(event => event.method === 'turn/completed'
            && event.params?.threadId === settingsThread && event.params.turn?.id === seed.turn.id)) return false;
        return (await request('thread/read', { threadId: settingsThread, includeTurns: true })).thread.turns
          .some(turn => turn.id === seed.turn.id && turn.status === 'completed');
      }, 10000, 'Private API-created root completes seed before CLI resume');
      if (settingsResume) {
        const status = async () => {
          const list = await request('mcpServerStatus/list', { threadId: settingsThread, serverName: 'mailbox_probe', detail: 'full' });
          assert.equal(list.data.length, 1);
          assert.equal(list.data[0].runtimeStatus, 'connected');
          const id = list.data[0].serverInfo.version;
          const sdk = await request('mcpServerStatus/list', { threadId: settingsThread, serverName: 'nexus_scode', detail: 'full' });
          assert.equal(sdk.data.length, 1);
          assert.equal(sdk.data[0].runtimeStatus, 'connected');
          assert.equal(sdk.data[0].serverInfo.name, 'nexus-scode');
          assert.equal(sdk.data[0].serverInfo.description, 'SCode registration ' + id);
          assert(Object.hasOwn(sdk.data[0].tools, 'scode_inbox'));
          return id;
        };
        assert.equal(await status(), expectedGeneration, 'Original native registration reports its launch generation');
        const originalGeneration = expectedGeneration;
        const originalRollout = fs.statSync(started.thread.path, { bigint: true });
        assert(originalRollout.isFile() && originalRollout.ino !== 0n);
        if (coldSettingsResume) {
          await request('thread/unsubscribe', { threadId: settingsThread });
          await wait(async () => {
            const loaded = await request('thread/loaded/list', {});
            assert.equal(loaded.nextCursor, null);
            return !loaded.data.includes(settingsThread);
          }, 10000, 'The original private thread unloads after its last subscriber leaves');
        }
        const requestedGeneration = crypto.randomUUID();
        const resumed = await request('thread/resume', { threadId: settingsThread,
          config: { 'mcp_servers.mailbox_probe.tools.scode_inbox.output_token_limit': 4096,
            'mcp_servers.mailbox_probe.env.NATIVE_MAILBOX_FIXTURE_GENERATION': requestedGeneration,
            'mcp_servers.nexus_scode': sdkRegistration(requestedGeneration, 4096) } });
        assert.equal(resumed.thread.id, settingsThread);
        const resumedRollout = fs.statSync(resumed.thread.path, { bigint: true });
        assert.deepEqual([resumedRollout.dev, resumedRollout.ino], [originalRollout.dev, originalRollout.ino],
          'Resume must select the same physical rollout file');
        assert(resumed.thread.turns.some(turn => turn.id === seed.turn.id), 'Existing seed turn survived resume');
        if (coldSettingsResume) expectedGeneration = requestedGeneration;
        const observedGeneration = await status();
        assert.equal(observedGeneration, expectedGeneration, 'Native reports the registration in use on the original thread');
        receipt.resumeRegistration = { originalGeneration, requestedGeneration, observedGeneration, seedTurnId: seed.turn.id,
          originalThread: settingsThread, resumedThread: resumed.thread.id, unloadedBeforeResume: coldSettingsResume };
        stage('existing-thread-registration-after-resume');
      }
    }
    const tuiAction = settingsProbe ? ['resume', settingsThread] : ['Reply with the isolated fixture seed.'];
    tui = spawn(driverArgv[0], [...driverArgv.slice(1), options.codex, '--no-alt-screen', '--cd', work, ...tuiAction],
      { cwd: work, env: { ...env, NATIVE_LIFECYCLE_PTY_TRACE: path.join(output, 'tui-trace.json') }, stdio: ['pipe', 'pipe', 'pipe'] });
    tuiExit = once(tui, 'close');
    tui.stdout.on('data', data => { tuiBuffer += data; let end; while ((end = tuiBuffer.indexOf('\n')) >= 0) {
      const line = tuiBuffer.slice(0, end); tuiBuffer = tuiBuffer.slice(end+1); tuiEvents.push(JSON.parse(line));
    } });
    tui.stderr.on('data', data => { tuiStderr = (tuiStderr+data).slice(-8000); });
    let thread;
    await wait(async () => {
      if (endpointFailure) throw new Error(endpointFailure);
      const inventory = await request('thread/loaded/list', {});
      assert.equal(inventory.nextCursor, null);
      if (!inventory.data.length) return false;
      // Metadata must be readable before the initial turn is persisted. Keep
      // full-history validation below, after the actual TUI renders completion.
      const candidates = [];
      for (const id of inventory.data) candidates.push((await request('thread/read', { threadId: id, includeTurns: false })).thread);
      const roots = candidates.filter(row => row.threadSource === 'user' && row.parentThreadId === null && row.ephemeral === false);
      // Native can create its own ephemeral title thread. Apply the same root
      // eligibility boundary as the owner, without enrolling the title worker.
      assert(roots.length <= 1, 'Private native has only one actual user TUI root');
      if (!roots.length) return false;
      thread = roots[0];
      const trace = path.join(output, 'tui-trace.json');
      return thread.status.type === 'idle' && fs.existsSync(trace)
        && JSON.parse(fs.readFileSync(trace)).outputTail.includes('Isolated native mailbox presentation completed.');
    }, 20000, 'Actual user root and completed seed');
    receipt.rootThread = thread;
    assert.equal(fs.realpathSync(thread.cwd), fs.realpathSync(work));
    assert.equal(thread.threadSource, 'user'); assert.equal(thread.parentThreadId, null);
    assert.equal(thread.ephemeral, false); assert.equal(thread.canAcceptDirectInput, true);
    stage('actual-root-eligible');
    if (settingsProbe) {
      assert.equal(thread.id, settingsThread, 'Actual TUI must resume the same configured root');
      // Compare the public config/inventory observations with the settings
      // supplied when creating this root. Never infer the
      // loaded thread's limit from a cwd-only config/read response.
      const diagnostics = await request('server/diagnostics', {});
      assert.equal(diagnostics.process.id, server.pid);
      const executableRows = process.platform === 'win32'
        ? [JSON.parse(execFileSync(probeArgv[0], [...probeArgv.slice(1), 'exe', String(server.pid)],
          { env, encoding: 'utf8', timeout: 3000 }))]
        : execFileSync('/usr/sbin/lsof', ['-a', '-p', String(server.pid), '-d', 'txt', '-Fn'],
          { encoding: 'utf8', timeout: 3000 }).split('\n').filter(line => line.startsWith('n')).map(line => line.slice(1));
      const executable = fs.realpathSync(options.codex);
      assert(executableRows.some(file => fs.realpathSync(file) === executable), 'Diagnostics PID must map to the measured native executable');
      const configuration = await request('config/read', { cwd: work, includeLayers: true });
      const inventory = await request('mcpServerStatus/list', { threadId: thread.id, serverName: 'mailbox_probe', detail: 'full' });
      if (settingsResume) {
        assert.equal(inventory.data[0].serverInfo.version, expectedGeneration);
        const sdk = await request('mcpServerStatus/list', { threadId: thread.id, serverName: 'nexus_scode', detail: 'full' });
        assert.equal(sdk.data[0].serverInfo.description, 'SCode registration ' + expectedGeneration);
        receipt.resumeRegistration.actualSdkDescription = sdk.data[0].serverInfo.description;
        await assert.rejects(sdkMeasurement.prepare(client, thread.id, crypto.randomUUID(), 4096), /no matching live/);
        sdkSample = await sdkMeasurement.prepare(client, thread.id, expectedGeneration, 4096);
        assert.equal(Buffer.byteLength(JSON.stringify({ content: [{ type: 'text', text: sdkSample.expectedText }] })), 4096);
        const refused = await request('mcpServer/tool/call', { threadId: thread.id, server: 'nexus_scode', tool: 'scode_inbox',
          arguments: { read: sdkSample.read, ack: ['private-fixture'] } });
        assert.equal(refused.isError, true);
        assert.equal(JSON.parse(refused.content[0].text).outcome, 'not_submitted');
      }
      receipt.settingsProbe = { diagnostics, executable, executableSha256: sha(executable),
        diskToolOutputTokens: 4096, threadStartOverrideToolOutputTokens: 64, configuration, inventory,
        boundary: 'Read-only loaded-thread settings diagnosis; no production admission or real mailbox ACK claim.' };
      stage('actual-owning-native-process-and-config-observed');
    }
    // Separate read-only observations retain unsupported native methods instead
    // of treating missing history as empty or retrying a failed delivery.
    receipt.historyDiagnostics = [];
    for (const [method, params] of [
      ['thread/queue/list', { threadId: thread.id, limit: 100 }],
      ['thread/turns/list', { threadId: thread.id, limit: 100, sortDirection: 'desc', itemsView: 'full' }],
      ['thread/items/list', { threadId: thread.id, limit: 100, sortDirection: 'desc' }],
      ['thread/read', { threadId: thread.id, includeTurns: true }],
    ]) {
      try { receipt.historyDiagnostics.push({ method, result: await request(method, params) }); }
      catch (error) { receipt.historyDiagnostics.push({ method, error: String(error) }); }
    }
    save();
    const traced = { ...client, request };
    const before = await readCodexMailboxSnapshot(traced, thread.id);
    assert(before.loaded && before.idle && before.queueEmpty && before.epoch !== 'none');
    receipt.before = { ...before, queued: [...before.queued], running: [...before.running], finished: [...before.finished] };
    stage('actual-native-inventory-compatible');
    if (realMailboxTruncation) {
      // The deliberate low-limit case must not admit a production wake owner.
      // Only this explicit fixture user turn asks to read; no peer body is in
      // that input. Tokens are chosen solely from actual model requests.
      await realMailbox.bind(traced, thread.id);
      phase = 'truncation';
      const params = { threadId: thread.id, input: [{ type: 'text', text: truncationPrompt }] };
      const result = await client.request('turn/start', params);
      receipt.fixtureActions.push({ method: 'turn/start', params, result });
      await wait(() => {
        if (endpointFailure) throw new Error(endpointFailure);
        return realModelCompleted;
      }, 20000, 'Actual native truncation recovers by complete read-alone windows');
      await wait(async () => {
        const view = await readCodexMailboxSnapshot(traced, thread.id);
        return view.idle && view.queueEmpty && view.epoch === result.turn.id;
      }, 10000, 'Private truncation user turn completes');
      await realMailbox.verifyTruncation();
      assert(!receipt.requests.some(row => ['thread/queue/add', 'turn/start', 'turn/steer'].includes(row.method)));
      receipt.toolEvents = fs.readFileSync(path.join(root, 'tool-events.jsonl'), 'utf8').trim().split('\n').map(JSON.parse);
      assert.equal(receipt.toolEvents.filter(row => row.method === 'tools/call').length, realToolSequence);
      assert.equal(realToolSequence, 12, 'One offer, five read/ACK pairs, one duplicate ACK');
      receipt.presentation = { deliberateNativeLimitTokens: 256, rawEncodedResultBound: encodedBound,
        sourceResultMaxBytes: Math.max(...receipt.realMailbox.tools.map(row => Buffer.byteLength(JSON.stringify(row.result)))),
        noPeerBodyInUserInput: true, nativeWakeSubmissions: 0 };
      assert(receipt.presentation.sourceResultMaxBytes <= encodedBound);
      stage('actual-native-truncated-offer-read-alone-before-five-commits');
      receipt.status = 'PASS_NATIVE_REAL_SCODE_INTERIOR_TRUNCATION_RECOVERY_ONLY';
      return;
    }
    if (activeReplyLoss || interrupted) {
      phase = 'active';
      const params = { threadId: thread.id, clientUserMessageId: 'fixture-human-active',
        input: [{ type: 'text', text: humanText }] };
      // Explicit fixture user action, separate from the production owner's RPCs.
      const result = await client.request('turn/start', params);
      receipt.fixtureActions.push({ method: 'turn/start', params, result });
      humanTurn = result.turn.id;
      await wait(async () => {
        const view = await readCodexMailboxSnapshot(traced, thread.id);
        return releaseHuman && !view.idle && view.running.has('fixture-human-active');
      }, 10000, 'Actual user turn held active');
      stage('actual-user-turn-held-active');
      if (interrupted) {
        const params = { threadId: thread.id, turnId: humanTurn };
        const result = await client.request('turn/interrupt', params);
        receipt.fixtureActions.push({ method: 'turn/interrupt', params, result });
        releaseHuman(); releaseHuman = undefined;
        await wait(async () => {
          const view = await readCodexMailboxSnapshot(traced, thread.id);
          return view.idle && view.finished.has('fixture-human-active');
        }, 10000, 'User interruption completes');
        const history = await request('thread/turns/list', { threadId: thread.id, limit: 100, itemsView: 'full', sortDirection: 'desc' });
        assert.equal(history.data.find(row => row.id === humanTurn)?.status, 'interrupted');
        stage('actual-user-interrupted-queue');
      }
    }
    const budget = mailboxSubmissionTextBudget('fixture-actor', thread.id, encodedBound);
    let text = 'OFFER-BEGIN-'+crypto.randomUUID()+'\n'+(sdkSample ? sdkSample.read+'\n' : '');
    while (Buffer.byteLength(text+'正文🙂"\\\n'+'OFFER-END') <= budget) text += '正文🙂"\\\n';
    text += 'OFFER-END'; expectedDelivery = CODEX_MAILBOX_DELIVERY_PROVENANCE+'\n'+text;
    if (wholeBudget) {
      assert.equal(budget, initialBudget);
      text = fillBudget('OFFER-BEGIN', 'OFFER-END', budget);
      expectedDelivery = CODEX_MAILBOX_DELIVERY_PROVENANCE+'\n'+text;
      assert.equal(Buffer.byteLength(text), budget); assert.equal(Buffer.byteLength(expectedWindow), budget);
    }
    receipt.presentation = { submissionBound: encodedBound, resultBound: encodedBound, offerTextBytes: Buffer.byteLength(text),
      windowTextBytes: Buffer.byteLength(expectedWindow), maxTextBytes: budget,
      encodedToolResultBytes: Buffer.byteLength(JSON.stringify({ content: [{ type: 'text', text: expectedWindow }], isError: false })),
      byteExactOfferRequired: true, byteExactWindowRequired: true };
    phase = 'delivery';
    const wakeOptions = { actor: 'fixture-actor', threadId: thread.id, journal: path.join(root, 'wake.jsonl'), native: traced,
      encodedSubmissionBytes: encodedBound,
      // Test-only permission to measure this native. This is not a production
      // admission certificate and does not assert untested queue semantics.
      qualified: async () => true,
      offer: async () => ({ heads: [{ conversation: 'fixture-peer', start: 0, deliveryId: 'fixture-delivery' }], text }) };
    if (ownerCrash) {
      const config = path.join(root, 'owner-fixture.json');
      fs.writeFileSync(config, JSON.stringify({ actor: wakeOptions.actor, threadId: thread.id, journal: wakeOptions.journal,
        encodedSubmissionBytes: encodedBound, offer: await wakeOptions.offer() }), { mode: 0o600, flag: 'wx' });
      const owner = spawn(process.execPath, [ownerFixture, internals, config],
        { cwd: work, env, stdio: ['ignore', 'ignore', 'pipe', 'ipc'] });
      const ownerExit = once(owner, 'close');
      let ownerFailure, stopped = false, ownerStderr = '';
      owner.stderr.on('data', data => { ownerStderr = (ownerStderr+data).slice(-8000); });
      owner.on('message', message => {
        void (async () => {
          assert(!message.unexpectedCompletion, 'Child owner must stop at its first queue-add boundary');
          if (message.method !== 'thread/queue/add') {
            const result = await request(message.method, message.params);
            if (owner.connected) owner.send({ id: message.id, result });
            return;
          }
          assert(!stopped, 'Only one submission before abrupt owner stop');
          const journal = fs.readFileSync(wakeOptions.journal, 'utf8');
          assert(journal.endsWith('\n'));
          const records = journal.trim().split('\n').map(JSON.parse);
          assert.equal(records.length, 1);
          assert.equal(records[0].value.n, 1);
          assert.equal(records[0].value.clientId, message.params.clientUserMessageId);
          assert.equal(message.status.state, 'submitted');
          assert.equal(message.status.submissions, 1);
          fs.writeFileSync(path.join(output, 'owner-before-crash.jsonl'), journal, { mode: 0o600, flag: 'wx' });
          receipt.requests.push({ method: message.method, params: message.params, stoppedBeforeForward: true });
          receipt.submission = message.status;
          receipt.ownerCrash = { pid: owner.pid, journalSha256: sha(wakeOptions.journal),
            nativeForwarded: false, abruptSignal: 'SIGKILL' };
          assert(owner.kill('SIGKILL'), 'Stop the exact owned controller child');
          stopped = true;
        })().catch(error => { ownerFailure = error; owner.kill('SIGKILL'); });
      });
      try {
        const [code, signal] = await bounded(ownerExit, 10000, 'Logged owner crash');
        if (ownerFailure) throw ownerFailure;
        assert(stopped && code === null && signal === 'SIGKILL', ownerStderr);
        assert.equal(sha(wakeOptions.journal), receipt.ownerCrash.journalSha256);
        receipt.fixtureActions.push({ action: 'stop-owned-controller-after-log-before-forward', pid: owner.pid, code, signal, joined: true });
      } finally {
        if (owner.exitCode === null && owner.signalCode === null) {
          owner.kill('SIGKILL'); await bounded(ownerExit, 3000, 'Failed child owner join');
        }
      }
      const view = await readCodexMailboxSnapshot(traced, thread.id);
      assert(view.idle && view.queueEmpty && !view.running.has(receipt.submission.clientId)
        && !view.finished.has(receipt.submission.clientId), 'Stopped submission never reached native');
      stage('actual-owner-killed-after-synced-slot-before-native-send');
    } else {
      wake = realMailboxCase ? await realMailbox.bind(traced, thread.id) : openCodexMailboxWake(wakeOptions);
      if (realMailboxCase) stage('real-scode-unread-preserved-without-ack');
      await wake.evaluate(); receipt.submission = wake.status();
      assert.equal(receipt.submission.state, activeReplyLoss || requestLoss ? 'blocked' : 'submitted', receipt.submission.error);
    }
    if (unknownSubmission) {
      if (requestLoss) assert.match(receipt.submission.error, /Fixture did not forward/);
      receipt.lostSubmission = receipt.submission;
      const firstHash = sha(wakeOptions.journal);
      if (wake) await wake.close();
      wake = openCodexMailboxWake(wakeOptions);
      await wake.evaluate(); assert.equal(wake.status().state, 'waiting');
      const start = performance.now();
      stage(ownerCrash ? 'actual-crashed-owner-reopened-real-delay-started' : 'actual-request-lost-owner-reopened-real-delay-started');
      // Use the real policy delay; neither the controller's clock nor interval
      // is replaced. There must be no second submission before 60 seconds.
      await new Promise(resolve => setTimeout(resolve, 55000));
      await wake.evaluate(); assert.equal(wake.status().state, 'waiting');
      assert.equal(sha(wakeOptions.journal), firstHash);
      assert.equal(receipt.requests.filter(row => row.method === 'thread/queue/add').length, 1);
      receipt.unresolvedAtMs = performance.now()-start;
      await new Promise(resolve => setTimeout(resolve, Math.max(0, 61000-(performance.now()-start))));
      // The interval may already have used the next number; never force an
      // extra evaluation after it has submitted.
      if (receipt.requests.filter(row => row.method === 'thread/queue/add').length === 1) await wake.evaluate();
      receipt.submission = wake.status();
      assert.equal(receipt.submission.state, 'submitted', receipt.submission.error);
      assert.equal(receipt.submission.submissions, 2);
      assert.notEqual(receipt.submission.clientId, receipt.lostSubmission.clientId);
      assert.equal(receipt.requests.filter(row => row.method === 'thread/queue/add').length, 2);
      receipt.nextSubmissionObservedMs = performance.now()-start;
      stage(ownerCrash ? 'actual-crashed-owner-next-number-after-real-delay' : 'actual-request-loss-next-number-after-real-delay');
    }
    if (activeReplyLoss) {
      assert.match(receipt.submission.error, /Fixture discarded accepted queue-add reply/);
      const queue = await request('thread/queue/list', { threadId: thread.id, limit: 100 });
      assert.equal(queue.nextCursor, null); assert.equal(queue.data.length, 1);
      const queued = queue.data[0];
      assert.equal(queued.clientUserMessageId, receipt.submission.clientId);
      assert.notEqual(queued.id, receipt.submission.clientId);
      assert.equal(queued.input[0].text, expectedDelivery);
      receipt.queuedWhileActive = queued;
      const journalHash = sha(wakeOptions.journal);
      await wake.evaluate(); assert.equal(wake.status().state, 'waiting');
      await wake.close(); wake = openCodexMailboxWake(wakeOptions);
      await wake.evaluate(); assert.equal(wake.status().state, 'waiting');
      assert.equal(sha(wakeOptions.journal), journalHash, 'Reconciliation and owner reopen must not spend another slot');
      assert.equal(receipt.requests.filter(row => row.method === 'thread/queue/add').length, 1);
      const active = await readCodexMailboxSnapshot(traced, thread.id);
      assert(active.running.has('fixture-human-active') && active.queued.has(receipt.submission.clientId));
      assert(!receipt.modelRequests.some(row => row.exactDeliverySeen), 'Delivery must wait for the user turn');
      stage('actual-active-queue-reply-loss-and-owner-reopen');
      releaseHuman(); releaseHuman = undefined;
    } else stage('actual-controller-queue-add-returned');
    if (interrupted) {
      // A bounded negative observation precedes the explicit fixture user start.
      // The production controller must never resume the interrupted queue.
      const until = Date.now()+2000;
      let observations = 0;
      do {
        await wake.evaluate(); assert.equal(wake.status().state, 'waiting');
        const view = await readCodexMailboxSnapshot(traced, thread.id);
        assert(view.idle && view.queued.has(receipt.submission.clientId) && !view.running.has(receipt.submission.clientId));
        assert(!receipt.modelRequests.some(row => row.exactDeliverySeen));
        observations++;
        await new Promise(resolve => setTimeout(resolve, 100));
      } while (Date.now() < until);
      receipt.interruptedQueueObservations = observations;
      assert.equal(receipt.requests.filter(row => row.method === 'thread/queue/add').length, 1);
      stage('actual-interrupted-delivery-remains-queued');
      const params = { threadId: thread.id };
      const result = await client.request('thread/queue/start', params);
      receipt.fixtureActions.push({ method: 'thread/queue/start', params, result });
    }
    await wait(() => {
      if (endpointFailure) throw new Error(endpointFailure);
      return receipt.modelRequests.some(row => row.exactDeliverySeen)
        && (!sdkSample || receipt.modelRequests.some(row => coldSettingsResume ? row.sdkSampleExact : row.sdkSampleTruncated))
        && (realMailboxCase ? realModelCompleted : settingsProbe && !coldSettingsResume
          ? receipt.modelRequests.some(row => row.truncatedWindowSeen) : receipt.modelRequests.some(row => row.exactWindowSeen));
    }, 20000, 'Native model receives exact offer and read-alone window');
    let after;
    await wait(async () => {
      after = await readCodexMailboxSnapshot(traced, thread.id);
      return after.idle && after.queueEmpty && after.finished.has(receipt.submission.clientId);
    }, 10000, 'Native finished history correlates submitted clientId');
    assert.equal(after.epoch, activeReplyLoss ? humanTurn : before.epoch, 'Peer delivery must not reset the non-wake epoch');
    receipt.after = { ...after, queued: [...after.queued], running: [...after.running], finished: [...after.finished] };
    if (unknownSubmission) {
      await wake.evaluate(); const third = wake.status();
      assert.equal(third.state, 'submitted', third.error); assert.equal(third.submissions, 3);
      await wait(async () => {
        const view = await readCodexMailboxSnapshot(traced, thread.id);
        return view.idle && view.queueEmpty && view.finished.has(third.clientId);
      }, 10000, 'Third submission finishes without an ACK');
      await wake.evaluate(); assert.equal(wake.status().state, 'delivery_exhausted');
      const exhaustedHash = sha(wakeOptions.journal);
      await wake.close(); wake = openCodexMailboxWake(wakeOptions);
      await wake.evaluate(); assert.equal(wake.status().state, 'delivery_exhausted');
      assert.equal(sha(wakeOptions.journal), exhaustedHash);
      assert.equal(receipt.requests.filter(row => row.method === 'thread/queue/add').length, 3);
      receipt.exhausted = wake.status();
      stage('actual-three-submission-exhaustion-survives-reopen');
      // Only a completed explicit fixture user turn opens a different epoch.
      phase = 'epoch';
      const params = { threadId: thread.id, clientUserMessageId: 'fixture-human-next-epoch',
        input: [{ type: 'text', text: 'Isolated next epoch '+crypto.randomUUID() }] };
      const result = await client.request('turn/start', params);
      receipt.fixtureActions.push({ method: 'turn/start', params, result });
      await wait(async () => {
        const view = await readCodexMailboxSnapshot(traced, thread.id);
        return view.idle && view.epoch === result.turn.id;
      }, 10000, 'Next non-delivery user epoch completes');
      await wake.evaluate(); const next = wake.status();
      assert.equal(next.state, 'submitted', next.error); assert.equal(next.submissions, 1);
      assert.notEqual(next.clientId, receipt.lostSubmission.clientId);
      await wait(async () => {
        const view = await readCodexMailboxSnapshot(traced, thread.id);
        return view.idle && view.queueEmpty && view.finished.has(next.clientId);
      }, 10000, 'Next epoch delivery finishes');
      receipt.nextEpochSubmission = next;
      const adds = receipt.requests.filter(row => row.method === 'thread/queue/add');
      assert.equal(adds.length, 4); assert.equal(new Set(adds.map(row => row.params.clientUserMessageId)).size, 4);
      receipt.submissionCounts = { ownerAttempts: 4, notForwarded: 1, acceptedByNative: 3 };
      stage('actual-completed-non-delivery-turn-opens-next-budget');
    }
    receipt.toolEvents = fs.readFileSync(path.join(root, 'tool-events.jsonl'), 'utf8').trim().split('\n').map(JSON.parse);
    assert.equal(receipt.toolEvents.filter(row => row.method === 'tools/call').length, realMailboxCase ? realToolSequence : 1);
    if (realMailboxCase) {
      await realMailbox.verify();
      await wake.evaluate(); assert.equal(wake.status().state, 'idle');
      assert.equal(receipt.requests.filter(row => row.method === 'thread/queue/add').length, 1);
      receipt.presentation.offerTextBytes = Buffer.byteLength(expectedDelivery)-Buffer.byteLength(CODEX_MAILBOX_DELIVERY_PROVENANCE+'\n');
      receipt.presentation.windowTextBytes = Math.max(...receipt.realMailbox.tools.map(row => Buffer.byteLength(row.result.content[0].text)));
      receipt.presentation.encodedToolResultBytes = Math.max(...receipt.realMailbox.tools.map(row => Buffer.byteLength(JSON.stringify(row.result))));
      stage(realMailboxBurst ? 'actual-native-real-scode-five-frame-burst-committed' : 'actual-native-real-scode-windows-and-single-ack-commit');
    }
    assert(!receipt.requests.some(row => ['turn/start', 'turn/steer', 'thread/queue/start'].includes(row.method)));
    receipt.presentation.encodedSubmissionBytes = Math.max(...receipt.requests.filter(row => row.method === 'thread/queue/add')
      .map(row => Buffer.byteLength(JSON.stringify(row.params))));
    assert(receipt.presentation.encodedSubmissionBytes <= encodedBound && receipt.presentation.encodedToolResultBytes <= encodedBound);
    if (wholeBudget) stage('actual-max-text-budget-preserved-with-json-escaping');
    if (settingsProbe) {
      if (sdkSample) {
        assert.equal(receipt.modelRequests.some(row => row.sdkSampleExact), coldSettingsResume);
        assert.equal(receipt.modelRequests.some(row => row.sdkSampleTruncated), !coldSettingsResume);
        const again = await request('mcpServer/tool/call', { threadId: thread.id, server: 'nexus_scode', tool: 'scode_inbox',
          arguments: { read: sdkSample.read } });
        assert(again.isError && JSON.parse(again.content[0].text).outcome === 'not_submitted');
        receipt.sdkPresentation = { encodedResultBytes: sdkSample.encodedResultBytes,
          registrationId: sdkSample.registrationId, connectionId: sdkSample.connectionId, actualThread: sdkSample.threadId,
          completeAtModel: coldSettingsResume, truncatedAtModel: !coldSettingsResume, duplicateRefused: true };
      }
      if (coldSettingsResume) {
        assert(receipt.modelRequests.some(row => row.exactWindowSeen), 'Cold-resumed original thread must present the complete tool output');
        assert(!receipt.modelRequests.some(row => row.truncatedWindowSeen));
      } else assert(!receipt.modelRequests.some(row => row.exactWindowSeen), 'Loaded thread must use its small override');
      receipt.presentation.byteExactWindowRequired = coldSettingsResume;
      receipt.settingsProbe.actualWindowTruncated = !coldSettingsResume;
      receipt.settingsProbe.observedConfigToolOutputTokens = receipt.settingsProbe.configuration.config
        ?.mcp_servers?.mailbox_probe?.tools?.scode_inbox?.output_token_limit ?? null;
      stage(coldSettingsResume ? 'actual-cold-resume-preserves-complete-tool-output' : 'actual-thread-override-truncates-tool-output');
    } else stage('actual-native-byte-exact-presentation-and-history');
    receipt.status = settingsProbe ? 'PASS_NATIVE_SETTINGS_DIAGNOSIS_ONLY'
      : realMailboxBurst ? 'PASS_NATIVE_REAL_SCODE_FIVE_FRAME_BURST_ONLY'
      : options.case === 'real-mailbox-restart' ? 'PASS_NATIVE_REAL_SCODE_RESTART_BEFORE_ACK_ONLY'
      : realMailboxCase ? 'PASS_NATIVE_REAL_SCODE_LOCAL_MAILBOX_ONLY'
      : activeReplyLoss ? 'PASS_NATIVE_ACTIVE_REPLY_LOSS_ONLY'
      : interrupted ? 'PASS_NATIVE_INTERRUPTED_QUEUE_ONLY'
      : requestLoss ? 'PASS_NATIVE_REQUEST_LOSS_BUDGET_ONLY'
      : ownerCrash ? 'PASS_NATIVE_LOGGED_OWNER_CRASH_BUDGET_ONLY'
      : wholeBudget ? 'PASS_NATIVE_MAX_TEXT_PRESENTATION_ONLY' : 'PASS_NATIVE_PRESENTATION_ONLY';
  } catch (error) {
    receipt.status = 'FAILED_OR_BLOCKED_NO_AUTOMATIC_RETRY'; receipt.firstFailure = String(error.stack ?? error); process.exitCode = 1;
  } finally {
    if (releaseHuman) { releaseHuman(); releaseHuman = undefined; }
    const cleanup = async (label, fn) => { try { await fn(); receipt.cleanup.push({ label, joined: true }); }
      catch (error) { receipt.cleanup.push({ label, error: String(error) }); receipt.status = 'FAILED_OR_BLOCKED_NO_AUTOMATIC_RETRY'; process.exitCode = 1; } };
    if (wake) await cleanup('controller', () => wake.close());
    if (realMailbox) await cleanup('real-scode-pair', () => realMailbox.close());
    if (client) await cleanup('proxy', () => client.close());
    if (tui) await cleanup('actual-tui', async () => {
      tui.stdin.end(JSON.stringify({ action: 'close' })+'\n');
      try { await bounded(tuiExit, 16000, 'TUI close'); }
      catch (error) { tui.kill('SIGTERM'); await bounded(tuiExit, 3000, 'TUI driver join'); throw error; }
      assert(tuiEvents.some(row => row.event === 'exit' && row.returncode === 0)
        && !tuiEvents.some(row => ['terminate', 'kill', 'normal_exit_timeout', 'unsafe_update_prompt'].includes(row.event)), 'TUI must close normally');
    });
    if (server) await cleanup('private-native-server', async () => {
      server.kill('SIGTERM');
      try { await bounded(serverExit, 5000, 'Native server close'); }
      catch (error) { server.kill('SIGKILL'); await bounded(serverExit, 3000, 'Native server forced join'); throw error; }
      receipt.privateNativeExit = { code: server.exitCode, signal: server.signalCode };
      // Windows implements SIGTERM with TerminateProcess. Joining this private
      // fixture child does not qualify a shared daemon's graceful shutdown.
      if (process.platform === 'win32') receipt.notProven = ['graceful native server shutdown'];
    });
    if (sdkOwner) await cleanup('packaged-sdk-owner', async () => { await shutdownCommand(); await sdkOwner; });
    endpoint.closeAllConnections(); await new Promise(resolve => endpoint.close(resolve));
    receipt.endpointFailure = endpointFailure; receipt.tuiEvents = tuiEvents; receipt.tuiStderr = tuiStderr; receipt.nativeStderr = nativeStderr;
    receipt.finishedAt = new Date().toISOString(); save(); console.log(receipt.status, path.join(output, 'receipt.json'));
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
