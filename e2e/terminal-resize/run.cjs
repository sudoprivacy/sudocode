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
const initial = config.scenario === 'budget' ? { cols: 80, rows: 18 }
  : config.scenario === 'parallel' ? { cols: 100, rows: 80 }
  : config.scenario === 'a2a' ? { cols: 100, rows: 50 } : { cols: 120, rows: 24 };
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
  SUDOCODE_INTERRUPT_QUEUE_MODE: 'queue', SUDOCODE_TODO_STORE: config.todos,
  SUDOCODE_MAX_TOOL_USE_CONCURRENCY: '10' };
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
  // The status counts assistant responses, including tool continuations. A
  // live peer acknowledgment may legitimately use send before its final reply.
  await settle(text => text.includes('│ QUEUED-BODY-END')
    && Number(text.match(/· turn (\d+) ·/)?.[1]) >= 2
    && text.split('\n').some(line => line.trim().replace(/^•\s*/, '') === config.expectedReply)
    && !text.includes('queued: Message from mac-ai'));
  const text = snapshot();
  assert.equal(text.split('\n').filter(line => line.trim() === '╭─ Message from mac-ai').length, 1, text);
  assert.equal(text.split('│ QUEUED-BODY-END').length - 1, 1, text);
  assert(!text.includes('<mailbox-message'), text);
  console.log('SCODE_XTERM_QUEUED_PEER_PASS');
}
async function parallelTools() {
  await settle(text => text.includes('❯'));
  child.write('\x1b[200~' + config.prompt + '\x1b[201~');
  await frame(text => text.includes('Pasted') || text.includes('TOOL_BATCH:')
    || text.includes('In one assistant'));
  child.write('\r');
  // The Rust producer marks readiness only after both FIFO readers have
  // opened. Neither gets a result until every resize assertion completes.
  await frame(() => fs.existsSync(path.join(config.root, 'parallel-ready')));
  for (const width of [52, 170, 66, 100]) {
    const started = Date.now();
    resize(width, 80);
    await frame(text => {
      const rows = text.split('\n');
      const starts = rows.flatMap((row, i) => row.startsWith('╭─ Bash(') ? [i] : []);
      return starts.length === 2 && rows.filter(row => row.trim() === '╰─').length === 2
        && starts.every(i => rows[i + 1]?.trim() === '╰─'
          && (width < 100 ? rows[i].endsWith('…') : rows[i].includes('resizes_END_')));
    }, started);
    child.write('x');
    await frame(text => text.split('\n').some(row => row.trimEnd() === '❯ x'));
    child.write('\x15');
    await frame(text => !text.split('\n').some(row => row.trimEnd() === '❯ x'));
    console.log(JSON.stringify({ scenario: 'parallel', width, pendingCards: 2, input: true }));
  }
  fs.writeFileSync(path.join(config.root, 'parallel-release'), 'release');
  await settle(text => text.includes('FIRST_OK') && text.includes('SECOND_OK')
    && text.split('\n').some(row => row.trim().replace(/^•\s*/, '') === 'Concurrency batch done.')
    && text.includes('ctx '));
  console.log('SCODE_XTERM_PARALLEL_TOOLS_PASS');
}
function viewport() {
  const buffer = terminal.buffer.active;
  return Array.from({ length: terminal.rows }, (_, i) =>
    buffer.getLine(buffer.baseY + i)?.translateToString(true) || '').join('\n');
}
function inputSlot() {
  // Tiny resizes can move an obsolete live frame into retained scrollback.
  // Locate the current editor between the last two input separators rather
  // than treating every old prompt visible after growth as an active editor.
  const rows = viewport().split('\n');
  const separator = '─'.repeat(terminal.cols);
  const end = rows.findLastIndex(row => row === separator);
  const start = rows.slice(0, end).findLastIndex(row => row === separator);
  if (start < 0 || end < 0 || !rows[start + 1]?.startsWith('❯')) return null;
  return rows.slice(start + 1, end).join('\n');
}
async function sharedBudget() {
  const payload = `DraftHead${'_keep_'.repeat(120)}DraftTail`;
  const command = `! echo ${payload} > draft.txt`;
  const history = label => {
    const text = snapshot();
    for (let i = 0; i < 70; i++)
      assert.equal(text.split('\n').filter(row => row.trim() === `Budget history line ${i}`).length,
        1, `${label}: history ${i} lost or duplicated\n${text}`);
    assert.equal(text.split('BudgetHistorySentinel').length - 1, 1, label);
  };
  await settle(() => viewport().includes('BudgetTask2') && viewport().includes('❯'));
  child.write(`\x1b[200~${command}\x1b[201~`);
  await settle(() => viewport().includes('compact') && inputSlot()?.includes('draft.txt'));
  assert(viewport().includes('3 todos'), viewport());
  assert(!viewport().includes('BudgetTask'), viewport());
  assert.equal(viewport().split('❯').length - 1, 1, viewport());
  history('folded');
  let started = Date.now();
  resize(80, 40);
  await settle(() => viewport().includes('BudgetTask2') && inputSlot()?.includes('DraftHead')
    && inputSlot()?.includes('draft.txt') && !viewport().includes('compact'), started);
  history('grown');
  // Move and edit in one burst before hiding the same editor.
  // Existing Up navigation reaches the start of this single logical line.
  // Then probe same-batch Home/middle insertion with the normal editor.
  child.write('\x1b[A');
  await settle(() => inputSlot()?.includes('❯ ! echo') && inputSlot()?.includes('DraftHead'));
  child.write('\x1b[H\x1b[C\x1b[C\x1b[C\x1b[C\x1b[CX');
  await settle(() => inputSlot()?.includes('! echXo'));
  child.write('\x7f');
  await settle(() => inputSlot()?.includes('❯ ! echo') && inputSlot()?.includes('DraftHead') && !inputSlot()?.includes('! echXo'));
  started = Date.now();
  resize(12, 8);
  await settle(() => viewport().includes('Enlarge') && viewport().includes('terminal'), started);
  child.write('MustNotAppear');
  await sleep(150);
  started = Date.now();
  resize(80, 40);
  await settle(() => inputSlot()?.includes('❯ ! echo') && inputSlot()?.includes('DraftHead') && viewport().includes('BudgetTask2'), started);
  assert(!snapshot().includes('MustNotAppear'), snapshot());
  child.write('Z');
  await settle(() => inputSlot()?.includes('! echZo'));
  child.write('\x7f');
  await settle(() => inputSlot()?.includes('❯ ! echo') && inputSlot()?.includes('DraftHead') && !inputSlot()?.includes('! echZo'));
  child.write('\r');
  const file = path.join(config.root, 'draft.txt');
  await settle(() => fs.existsSync(file) && snapshot().includes('(no output)')
    && inputSlot()?.trimEnd() === '❯');
  assert.equal(fs.readFileSync(file, 'utf8').trim(), payload, 'all draft bytes must survive');
  history('submitted');
  console.log('SCODE_XTERM_SHARED_BUDGET_PASS');
}
async function run() {
  try {
    if (config.scenario === 'budget') {
      await sharedBudget();
    } else if (config.scenario === 'a2a') {
      await queuedPeer();
    } else if (config.scenario === 'parallel') {
      await parallelTools();
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
