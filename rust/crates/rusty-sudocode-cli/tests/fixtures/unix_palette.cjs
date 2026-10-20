// Answer startup queries from the ready terminal peer, before foreground waits.
const fs = require('node:fs');
const assert = require('node:assert/strict');
const { performance } = require('node:perf_hooks');
const { createHost } = require('../../../../../e2e/terminal-resize/host.cjs');
const config = JSON.parse(fs.readFileSync(process.argv[2], 'utf8'));
assert(['early-input', 'light-override'].includes(config.scenario));
for (const budget of [config.defaultTimeoutMs, config.turnTimeoutMs]) {
  assert(Number.isSafeInteger(budget) && budget > 0, 'Rust supplies the shared PTY budgets');
}
const host = createHost({ ...config, cols: 110, rows: 100, backend: 'bundled' });
const { child, terminal, state, snapshot, close } = host;
const frame = predicate => host.wait(predicate, { timeoutMs: config.defaultTimeoutMs });
const queries = [];
let firstQuery;
let replyElapsedMs;
for (const command of [10, 11]) {
  terminal.parser.registerOscHandler(command, data => {
    if (data !== '?') return false;
    firstQuery ??= performance.now();
    queries.push(command);
    if (queries.length === 2) {
      if (config.scenario === 'early-input') {
        // Fragment a real reply and interleave actual keys and bracketed paste.
        child.write('early-\x1b]10;rgb:ffff/');
        child.write("ffff/ffff\x07\x1b[200~paste '$HOME' ~/.zshrc\x1b[201~");
        child.write('\x1b]11;rgb:0000/0000/0000\x1b\\');
      } else {
        child.write('\x1b]10;rgb:0000/0000/0000\x07\x1b]11;rgb:ffff/ffff/ffff\x07');
      }
      replyElapsedMs = performance.now() - firstQuery;
    }
    return true;
  });
}
function exactInput(text, draft) {
  return text.split('\n').some(row => row.trimEnd() === `❯ ${draft}`.trimEnd());
}
function inlineColor() {
  const buffer = terminal.buffer.active;
  for (let row = buffer.length - 1; row >= 0; row--) {
    const line = buffer.getLine(row);
    const column = line.translateToString(true).indexOf('source ~/.zshrc');
    if (column >= 0) {
      const cell = line.getCell(column);
      return { rgb: cell.isFgRGB(), foreground: cell.getFgColor(), defaultBackground: cell.isBgDefault() };
    }
  }
}
async function run() {
  try {
    if (config.scenario === 'early-input') {
      await frame(text => exactInput(text, "early-paste '$HOME' ~/.zshrc"));
      child.write('\x15');
      await frame(text => exactInput(text, ''));
      child.write('\x1b]11;rgb:ffff/ffff/ffff\x07\x1b]10;rgb:bad!\x07after');
      await frame(text => exactInput(text, 'after'));
      assert(!snapshot().includes('rgb:'), 'late/malformed replies must not enter the draft');
      child.write('\x15');
      await frame(text => exactInput(text, ''));
    } else {
      await frame(text => exactInput(text, ''));
      child.write(`\x1b[200~${config.prompt}\x1b[201~`);
      const folded = `[Pasted text #1 +${config.prompt.split('\n').length - 1} lines]`;
      await frame(text => exactInput(text, folded));
      child.write('\r');
      await host.wait(text => text.includes('Color reference done.') && text.includes('ctx ')
        && exactInput(text, '') && inlineColor() !== undefined, { timeoutMs: config.turnTimeoutMs });
      assert.deepEqual(inlineColor(), { rgb: true, foreground: 0x40a02b, defaultBackground: true },
        `detected light background must override dark COLORFGBG\n${snapshot()}`);
    }
    assert.deepEqual([...queries].sort((a, b) => a - b), [10, 11], 'one foreground/background query');
    child.write('/exit');
    await frame(text => exactInput(text, '/exit'));
    child.write('\r');
    const exitDeadline = performance.now() + config.defaultTimeoutMs;
    while (performance.now() < exitDeadline && !state.exited) await host.sleep(20);
    assert(state.exited, 'scode did not exit');
    assert.equal(state.exitCode, 0);
    console.log(JSON.stringify({ scenario: config.scenario, queries, replyElapsedMs }));
    console.log('SCODE_UNIX_PALETTE_PASS');
  } finally {
    await close();
  }
}
run().then(() => process.exit(0)).catch(error => { console.error(error); process.exit(1); });
