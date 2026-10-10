// Test-specific scenario; transport and tracing use the shared terminal host.
const fs = require('node:fs');
const assert = require('node:assert/strict');
const { createHost } = require('../../../../../e2e/terminal-resize/host.cjs');
const config = JSON.parse(fs.readFileSync(process.argv[2], 'utf8'));
fs.writeFileSync(config.todos, JSON.stringify([
  { content: 'PaletteSentinel', activeForm: 'Checking palette', status: 'pending' },
]));
const host = createHost({ ...config, cols: 120, rows: 40 });
const { child, terminal, state, settle, snapshot, close } = host;
const queries = [], replies = [];
let replyTimer;
for (const command of [10, 11]) {
  terminal.parser.registerOscHandler(command, data => {
    if (data !== '?') return false;
    queries.push(command);
    // Model a terminal whose answer arrives after the startup probe's deadline.
    replies.push(`\x1b]${command};rgb:ffff/ffff/ffff\x1b\\`);
    return true;
  });
}
function exactInput(text, draft) {
  return text.split('\n').some(row => row.trimEnd() === `❯ ${draft}`);
}
function mutedLabel() {
  const buffer = terminal.buffer.active;
  for (let row = buffer.length - 1; row >= 0; row--) {
    const line = buffer.getLine(row), text = line.translateToString(true);
    const col = text.indexOf('todos');
    if (col >= 0) return line.getCell(col);
  }
  return undefined;
}
async function run() {
  try {
    await settle(text => exactInput(text, '') && text.includes('PaletteSentinel'));
    child.write('PaletteDraft');
    // If queried, respond while the user's draft is active, not during setup.
    replyTimer = setTimeout(() => {
      for (const reply of replies) child.write(reply);
    }, 350);
    await settle(text => exactInput(text, 'PaletteDraft'));
    await host.sleep(400);
    await settle(text => exactInput(text, 'PaletteDraft'));
    const text = snapshot();
    assert(!text.includes('rgb:') && !text.includes('ffff/'), `color reply leaked into UI\n${text}`);
    assert.deepEqual(queries, [], 'Windows must not solicit OSC 10/11 replies');
    const label = mutedLabel();
    assert(label?.isFgPalette(), `Todo label must use the fallback palette\n${text}`);
    assert.equal(label.getFgColor(), config.expectedMuted, 'COLORFGBG theme selection');
    child.write('\x15/exit');
    await settle(text => exactInput(text, '/exit'));
    child.write('\r');
    for (let i = 0; i < 250; i++) {
      if (state.exitCode === undefined) state.exitCode = child._agent?.exitCode;
      if (state.exited && state.exitCode !== undefined && !state.pending) break;
      await host.sleep(20);
    }
    assert(state.exited, 'scode did not exit');
    assert.equal(state.exitCode, 0);
    console.log(JSON.stringify({ backend: config.backend, background: config.env.COLORFGBG,
      queries, input: 'PaletteDraft', muted: label.getFgColor() }));
    console.log('SCODE_WINDOWS_PALETTE_PASS');
  } finally {
    clearTimeout(replyTimer);
    await close();
  }
}
run().then(() => process.exit(0)).catch(error => { console.error(error); process.exit(1); });
