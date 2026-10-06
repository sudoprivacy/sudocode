// Use the real terminal's reflow and retain its complete scrollback buffer.
const path = require('node:path');
const fs = require('node:fs');
const os = require('node:os');
const assert = require('node:assert/strict');
const modules = process.env.SCODE_TERMINAL_MODULES;
const host = name => require(modules ? path.join(modules, name) : name);
const pty = host('node-pty');
const { Terminal } = host('@xterm/headless');
const { Unicode11Addon } = host('@xterm/addon-unicode11');
const config = JSON.parse(fs.readFileSync(process.argv[2], 'utf8'));
assert(['bundled', 'system'].includes(config.backend), 'unknown ConPTY backend');
const initial = config.scenario === 'a2a' ? { cols: 100, rows: 50 } : { cols: 120, rows: 24 };
const terminal = new Terminal({
  ...initial, scrollback: 10000, allowProposedApi: true,
  reflowCursorLine: config.backend !== 'system',
  windowsPty: process.platform === 'win32'
    ? { backend: 'conpty', buildNumber: Number(os.release().split('.')[2]) } : undefined,
});
terminal.loadAddon(new Unicode11Addon());
terminal.unicode.activeVersion = '11';
console.log(JSON.stringify({ platform: process.platform, os: os.release(),
  node: process.versions.node, electron: process.versions.electron,
  pty: host('node-pty/package.json').version,
  xterm: host('@xterm/headless/package.json').version, backend: config.backend }));
const env = { ...process.env, SUDO_CODE_CONFIG_HOME: config.configHome,
  HOME: path.join(config.root, 'home'), TERM: 'xterm-256color', COLORTERM: 'truecolor',
  SUDOCODE_INTERRUPT_QUEUE_MODE: 'queue', SUDOCODE_TODO_STORE: config.todos };
for (const key of ['ELECTRON_RUN_AS_NODE', 'NO_COLOR', 'SCODE_GLOBAL_CONFIG_DIR', 'SCODE_PROJECT_CONFIG_DIR'])
  delete env[key];
const child = pty.spawn(config.binary, config.args, { cwd: config.root, env,
  ...initial, useConpty: true, useConptyDll: config.backend === 'bundled' });
const wire = [];
let pending = 0, lastData = Date.now(), exited = false, exitCode;
child.onData(data => {
  wire.push({ data, time: Date.now() });
  pending++; lastData = Date.now();
  terminal.write(data, () => pending--);
});
terminal.onData(data => child.write(data));
if (process.platform === 'win32') terminal.parser.registerCsiHandler({ final: 'c' }, params => {
  if (!params.length || (params.length === 1 && params[0] === 0)) {
    child.write('\x1b[?61;4c'); return true;
  }
  return false;
});
child.onExit(event => { exited = true; exitCode = event.exitCode; });
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
function snapshot() {
  const buffer = terminal.buffer.active;
  return Array.from({ length: buffer.length }, (_, i) => buffer.getLine(i).translateToString(true)).join('\n');
}
async function settle(predicate, notBefore = 0) {
  const deadline = Date.now() + 20000;
  while (Date.now() < deadline) {
    await sleep(25);
    if (!pending && lastData >= notBefore && Date.now() - lastData >= 300
      && predicate(snapshot())) return;
    assert(!exited, `scode exited before the expected frame\n${snapshot()}`);
  }
  throw new Error(`terminal did not settle\n${snapshot()}`);
}
async function frame(predicate, notBefore = 0) {
  // A running turn animates continuously. Wait for parsed output satisfying
  // the complete frame condition without requiring that animation to stop.
  const deadline = Date.now() + 45000;
  while (Date.now() < deadline) {
    await sleep(25);
    if (!pending && lastData >= notBefore && predicate(snapshot())) return;
    assert(!exited, `scode exited before the expected frame\n${snapshot()}`);
  }
  throw new Error(`expected terminal frame was not drawn\n${snapshot()}`);
}
function check(label) {
  const text = snapshot();
  for (const marker of ['turn 1', '1 todos', 'ResizeCompletedTask', 'ResizeHistorySentinel', 'DraftSurvivesResize'])
    assert.equal(text.split(marker).length - 1, 1, `${label}: ${marker}\n${text}`);
  for (let i = 0; i < 70; i++)
    assert.equal(text.split('\n').filter(line => line.trim() === `Earlier history line ${i}`).length,
      1, `${label}: history ${i} was lost or duplicated\n${text}`);
  console.log(JSON.stringify({ label, history: 70, chrome: 1, draft: true }));
}
function resize(cols, rows) {
  // Keep resize and tracing free of synchronous disk I/O.
  wire.push({ resize: [cols, rows], time: Date.now() });
  terminal.resize(cols, rows); child.resize(cols, rows);
}
async function queuedPeer() {
  await settle(text => text.includes('❯'));
  child.write(config.prompt + '\r');
  await frame(() => fs.existsSync(path.join(config.root, 'cancel-ready')));
  // The Rust fixture now injects through the real Mailbox API.
  await frame(text => text.includes('queued: Message from mac-ai'));
  let started = Date.now();
  resize(42, 60);
  await frame(text => text.split('\n').some(line => line.includes('queued: Message from mac-ai')
    && line.endsWith('…')) && !text.includes('QUEUED-BODY-END'), started);
  child.write('draft-中文');
  await frame(text => text.includes('❯ draft-中文'));
  started = Date.now();
  resize(180, 60);
  await frame(text => text.split('\n').some(line => line.includes('queued: Message from mac-ai')
    && line.includes(config.firstLine)) && text.includes('❯ draft-中文'), started);
  child.write('\x15');
  await frame(text => !text.includes('draft-中文'));
  child.write('\x1b');
  await settle(text => text.includes('│ QUEUED-BODY-END') && text.includes('turn 2')
    && text.split('\n').some(line => line.trim().replace(/^•\s*/, '') === config.expectedReply)
    && !text.includes('queued: Message from mac-ai'));
  const text = snapshot();
  assert.equal(text.split('\n').filter(line => line.trim() === '╭─ Message from mac-ai').length, 1, text);
  assert.equal(text.split('│ QUEUED-BODY-END').length - 1, 1, text);
  assert(!text.includes('<mailbox-message'), text);
  console.log('SCODE_XTERM_QUEUED_PEER_PASS');
}
async function run() {
  try {
    if (config.scenario === 'a2a') {
      await queuedPeer();
    } else {
      await settle(text => text.includes('turn 1') && text.includes('❯'));
      child.write('DraftSurvivesResize');
      await settle(text => text.includes('DraftSurvivesResize'));
      check('initial');
      for (const [cols, rows] of [[240,40], [100,40], [240,40], [80,40], [240,40],
        [60,40], [240,40], [60,18], [240,40]]) {
        const started = Date.now();
        resize(cols, rows);
        await settle(text => text.includes('DraftSurvivesResize')
          && text.split('\n').some(line => line === '─'.repeat(cols)), started);
        check(`${cols}x${rows}`);
      }
      const rapidStarted = Date.now();
      for (const cols of [200, 90, 140, 70, 240]) { resize(cols, 40); await sleep(10); }
      await settle(text => text.includes('DraftSurvivesResize') && text.includes('─'.repeat(240)), rapidStarted);
      check('rapid');
      child.write('\x15');
      await settle(text => !text.includes('DraftSurvivesResize'));
    }
    child.write('/exit');
    await settle(text => text.includes('❯ /exit'));
    child.write('\r');
    for (let i = 0; i < 250; i++) {
      if (exitCode === undefined && process.platform === 'win32') exitCode = child._agent?.exitCode;
      if (exited && exitCode !== undefined && !pending) break;
      await sleep(20);
    }
    assert(exited, 'scode did not exit');
    assert.equal(exitCode, 0);
    console.log('SCODE_XTERM_RESIZE_PASS');
  } finally {
    // Even a failed assertion should let the owned REPL release ConPTY.
    // Closing an active native terminal immediately can hang host teardown.
    if (!exited) {
      child.write('\x15/exit\r');
      for (let i = 0; i < 100 && !exited; i++) await sleep(20);
    }
    if (!exited) child.kill();
    fs.mkdirSync(config.logRoot, { recursive: true });
    fs.writeFileSync(path.join(config.logRoot, 'wire.jsonl'), wire.map(row => JSON.stringify(row)).join('\n'));
    fs.writeFileSync(path.join(config.logRoot, 'terminal.txt'), snapshot());
    terminal.dispose();
  }
}
run().then(() => process.exit(0)).catch(error => { console.error(error); process.exit(1); });
