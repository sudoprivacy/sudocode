// Shared real-terminal transport, observation and failure artifacts.
const path = require('node:path');
const fs = require('node:fs');
const os = require('node:os');
const assert = require('node:assert/strict');
const { performance } = require('node:perf_hooks');

const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));

function createHost(config) {
  const modules = process.env.SCODE_TERMINAL_MODULES;
  const load = name => require(modules ? path.join(modules, name) : name);
  assert(['bundled', 'system'].includes(config.backend), 'unknown ConPTY backend');
  const initial = { cols: config.cols || 120, rows: config.rows || 40 };
  const terminal = new (load('@xterm/headless').Terminal)({
    ...initial, scrollback: 10000, allowProposedApi: true,
    reflowCursorLine: config.backend !== 'system',
    windowsPty: process.platform === 'win32'
      ? { backend: 'conpty', buildNumber: Number(os.release().split('.')[2]) } : undefined,
  });
  terminal.loadAddon(new (load('@xterm/addon-unicode11').Unicode11Addon)());
  terminal.unicode.activeVersion = '11';
  const metadata = { platform: process.platform, os: os.release(), arch: process.arch,
    node: process.versions.node, electron: process.versions.electron,
    pty: load('node-pty/package.json').version,
    xterm: load('@xterm/headless/package.json').version, backend: config.backend };
  console.log(JSON.stringify(metadata));
  const env = { ...process.env, SUDO_CODE_CONFIG_HOME: config.configHome,
    HOME: path.join(config.root, 'home'), TERM: 'xterm-256color', COLORTERM: 'truecolor',
    COLORFGBG: '15;0', SUDOCODE_INTERRUPT_QUEUE_MODE: 'queue',
    SUDOCODE_TODO_STORE: config.todos, SUDOCODE_MAX_TOOL_USE_CONCURRENCY: '10',
    ...config.env };
  for (const key of ['ELECTRON_RUN_AS_NODE', 'NO_COLOR', 'SCODE_GLOBAL_CONFIG_DIR', 'SCODE_PROJECT_CONFIG_DIR'])
    delete env[key];
  const child = load('node-pty').spawn(config.binary, config.args, { cwd: config.root, env,
    ...initial, useConpty: true, useConptyDll: config.backend === 'bundled' });
  const wire = [], presentations = new Set(), parsed = new Set();
  const state = { pending: 0, lastData: Date.now(), exited: false, exitCode: undefined,
    bytes: 0, frames: 0 };
  child.onData(data => {
    wire.push({ data, time: Date.now() });
    state.bytes += Buffer.byteLength(data);
    state.pending++; state.lastData = Date.now();
    terminal.write(data, () => {
      state.pending--;
      for (const observe of parsed) observe();
    });
  });
  terminal.parser.registerCsiHandler({ prefix: '?', final: 'l' }, params => {
    if (params.includes(2026)) {
      state.frames++;
      for (const observe of presentations) observe();
    }
    return false;
  });
  terminal.onData(data => child.write(data));
  if (process.platform === 'win32') terminal.parser.registerCsiHandler({ final: 'c' }, params => {
    if (!params.length || (params.length === 1 && params[0] === 0)) {
      child.write('\x1b[?61;4c'); return true;
    }
    return false;
  });
  child.onExit(event => { state.exited = true; state.exitCode = event.exitCode; });
  function snapshot() {
    const buffer = terminal.buffer.active;
    return Array.from({ length: buffer.length }, (_, i) => buffer.getLine(i).translateToString(true)).join('\n');
  }
  function viewport() {
    const buffer = terminal.buffer.active;
    return Array.from({ length: terminal.rows }, (_, i) => buffer.getLine(buffer.baseY + i)
      .translateToString(true)).join('\n');
  }
  async function wait(predicate, { notBefore = 0, quietMs = 0, timeoutMs = 45000, pollMs = 25 } = {}) {
    const deadline = performance.now() + timeoutMs;
    while (performance.now() < deadline) {
      if (!state.pending && state.lastData >= notBefore && Date.now() - state.lastData >= quietMs
        && predicate(snapshot())) return;
      assert(!state.exited, `scode exited before the expected frame\n${snapshot()}`);
      await sleep(pollMs);
    }
    throw new Error(`expected terminal frame was not drawn\n${snapshot()}`);
  }
  function resize(cols, rows) {
    wire.push({ resize: [cols, rows], time: Date.now() });
    terminal.resize(cols, rows); child.resize(cols, rows);
  }
  async function resizeFrame(cols, rows, predicate, options) {
    const framesBefore = state.frames;
    resize(cols, rows);
    // Reflow and cursor-position replies can satisfy text predicates before
    // the app redraws. Require a completed frame at the requested width.
    await wait(text => state.frames > framesBefore
      && viewport().split('\n').some(row => row === '─'.repeat(cols))
      && predicate(text), options);
  }
  function subscribe(listeners, observer) {
    listeners.add(observer);
    return () => listeners.delete(observer);
  }
  async function close() {
    if (!state.exited) {
      child.write('\x15/exit\r');
      for (let i = 0; i < 100 && !state.exited; i++) await sleep(20);
    }
    if (!state.exited) child.kill();
    fs.mkdirSync(config.logRoot, { recursive: true });
    fs.writeFileSync(path.join(config.logRoot, 'wire.jsonl'), wire.map(row => JSON.stringify(row)).join('\n'));
    fs.writeFileSync(path.join(config.logRoot, 'terminal.txt'), snapshot());
    terminal.dispose();
  }
  return { child, terminal, state, metadata, sleep, snapshot, viewport, wait, resize, resizeFrame, close,
    settle: (predicate, notBefore = 0) => wait(predicate, { notBefore, quietMs: 300, timeoutMs: 20000 }),
    frame: (predicate, notBefore = 0) => wait(predicate, { notBefore }),
    onPresentation: observer => subscribe(presentations, observer),
    onParsed: observer => subscribe(parsed, observer) };
}

module.exports = { createHost };
