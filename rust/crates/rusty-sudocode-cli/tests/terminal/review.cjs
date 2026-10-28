// Native history owns the review document; the application owns its controls.
const fs = require('node:fs');
const path = require('node:path');
const assert = require('node:assert/strict');
const { createHost } = require('../../../../../e2e/terminal-resize/host.cjs');

function countLines(host, expected, kind) {
  const lines = host.snapshot().split('\n');
  const bullet = kind === 'plan' ? /^\s*│\s*[•*-]\s/ : /^\s*[•*-]\s/;
  return Array.from({ length: expected }, (_, i) => lines.filter(line =>
    bullet.test(line) && line.includes(`ReviewLine${String(i).padStart(3, '0')}:`)).length);
}

function documentStyles(host, config) {
  const expected = [
    ['Full review document', !config.isNoColor, false],
    ['ReviewLine000', !config.isNoColor, false],
    ['src/main.rs', false, !config.isNoColor],
  ];
  if (config.kind === 'plan') expected.push(['Context', !config.isNoColor, false]);
  const buffer = host.terminal.buffer.active;
  return expected.map(([needle, bold, colored]) => {
    const matches = [];
    for (let row = 0; row < buffer.length; row++) {
      const line = buffer.getLine(row), text = line.translateToString(true);
      const body = config.kind === 'plan' ? text.match(/^\s*│\s?(.*)$/)?.[1] : text;
      if (body === undefined) continue;
      const content = body.trim();
      if (needle === 'ReviewLine000'
        ? !/^[•*-]\s+ReviewLine000:/.test(content) : content !== needle) continue;
      const at = text.indexOf(needle);
      let offset = 0;
      for (let col = 0; col < host.terminal.cols; col++) {
        const cell = line.getCell(col);
        const chars = cell.getChars() || (cell.getWidth() === 0 ? '' : ' ');
        if (chars && offset === at) {
          matches.push({ bold: Boolean(cell.isBold()), colored: cell.getFgColorMode() !== 0 });
          break;
        }
        offset += chars.length;
      }
    }
    assert.deepEqual(matches, [{ bold, colored }],
      `${needle}: review styles survive history, resize and inspection (NoColor=${config.isNoColor})`);
    return { needle, ...matches[0] };
  });
}

async function review(host, config) {
  const hasControls = () => host.viewport().split('\n').some(line => config.kind === 'plan'
    ? /^\s*\[2\] Keep context & execute/.test(line)
    : line.startsWith('1/2  First review decision'));
  const hasStreamTail = () => host.snapshot().split('\n').some(line =>
    /^\s*(?:•\s*)?ReviewStreamPulse:/.test(line));
  await host.frame(() => host.viewport().includes('❯'));
  host.child.write(`\x1b[200~${config.prompt}\x1b[201~`);
  await host.frame(() => host.viewport().includes('Pasted'));
  host.child.write('\r');
  await host.frame(hasControls);
  const planPath = path.join(config.root, 'review-plan.md');
  const frozenPlan = config.kind === 'plan' ? fs.readFileSync(planPath, 'utf8') : null;
  const history = () => {
    const lines = host.snapshot().split('\n');
    for (let i = 0; i < 70; i++) assert.equal(lines.filter(line =>
      line.trim() === `EarlierReviewHistory${String(i).padStart(3, '0')}`).length, 1,
      'resumed history survives review');
    assert.deepEqual(countLines(host, config.expected, config.kind), Array(config.expected).fill(1),
      `every rendered line is available exactly once\n${host.snapshot()}`);
    documentStyles(host, config);
  };
  history();
  const inspect = async () => {
    const capture = path.join(config.root, 'pager-source.txt');
    fs.rmSync(capture, { force: true });
    let isPresented = false;
    const stopWatching = host.onPresentation(() => {
      isPresented = fs.existsSync(capture) && hasControls();
    });
    try {
      host.child.write('\x0f');
      await host.frame(() => isPresented);
    } finally { stopWatching(); }
    const source = fs.readFileSync(capture, 'utf8');
    for (let i = 0; i < config.expected; i++)
      assert.equal(source.split(`ReviewLine${String(i).padStart(3, '0')}`).length - 1, 1,
        'the pager receives the entire frozen document');
    history();
  };
  // Move the host's native viewport as a user's wheel would, without sending
  // application mouse or PageUp events to the question's controls.
  host.terminal.scrollLines(-20);
  const reading = host.terminal.buffer.active.viewportY;
  const beforeReading = { bytes: host.state.bytes, frames: host.state.frames };
  assert(reading < host.terminal.buffer.active.baseY, 'a real history reading position');
  if (!config.isLive) {
    // The common slot allocator gives decisions priority over unfinished
    // prose. Release the stream now and verify its complete tail after review.
    assert(!hasStreamTail(), 'the provider tail is still gated');
    fs.writeFileSync(path.join(config.root, 'review-stream-release'), 'release');
  }
  await host.sleep(600);
  assert.equal(host.terminal.buffer.active.viewportY, reading,
    'status updates must not pull the reader back to the prompt');
  const readingUpdates = { bytes: host.state.bytes - beforeReading.bytes,
    frames: host.state.frames - beforeReading.frames };
  history();
  host.terminal.scrollToBottom();
  for (const [cols, rows] of [[100, 24], [80, 24], [120, 40]]) {
    await host.resizeFrame(cols, rows, hasControls);
    history();
  }
  await inspect();
  if (config.kind === 'plan') {
    host.child.write('Reviewed every line');
    await host.frame(() => host.viewport().includes('Reviewed every line'));
    history();
    await inspect();
    assert(host.viewport().includes('Reviewed every line'), 'pager return preserves feedback');
    host.child.write('\x1b[A');
    await host.frame(() => host.viewport().split('\n').some(line =>
      line.startsWith('> [4] Exit plan')));
    host.child.write('\r');
  } else {
    host.child.write('\r');
    await host.frame(() => host.viewport().split('\n').some(line =>
      line.startsWith('12 items  Second review decision')));
    // One immutable request description is shared across both picker types.
    history();
    host.child.write('\r');
  }
  await host.frame(() => host.viewport().includes('❯')
    && host.snapshot().includes(config.isLive ? 'Review finished.' : 'Concurrency batch done.')
    && (config.isLive || hasStreamTail()));
  history();
  fs.mkdirSync(config.logRoot, { recursive: true });
  if (frozenPlan !== null) assert.equal(fs.readFileSync(planPath, 'utf8'), frozenPlan,
    'inspection and rejection do not rewrite the frozen review source');
  fs.writeFileSync(path.join(config.logRoot, 'review-result.json'), JSON.stringify({
    kind: config.kind, no_color: config.isNoColor, lines: config.expected, reading_position: reading,
    reading_updates: readingUpdates,
    after: countLines(host, config.expected, config.kind),
    styles: documentStyles(host, config),
  }, null, 2));
}

(async () => {
  const config = JSON.parse(process.argv[2]);
  const pager = path.join(config.root, 'review-pager.cjs');
  fs.writeFileSync(pager, `const fs=require('node:fs'),path=require('node:path');
let text='';process.stdin.setEncoding('utf8');process.stdin.on('data',chunk=>text+=chunk);
process.stdin.on('end',()=>{fs.writeFileSync(path.join(__dirname,'pager-source.txt'),text);
console.log('Full frozen review inspected.');});`);
  config.env.PAGER = `node '${pager.replace(/'/g, "'\\''")}'`;
  const host = createHost(config);
  try {
    await review(host, config);
    host.child.write('/exit');
    await host.frame(() => host.viewport().includes('❯ /exit'));
    host.child.write('\r');
    for (let i = 0; i < 250; i++) {
      if (host.state.exitCode === undefined && process.platform === 'win32')
        host.state.exitCode = host.child._agent?.exitCode;
      if (host.state.exited && host.state.exitCode !== undefined && !host.state.pending) break;
      await host.sleep(20);
    }
    assert(host.state.exited, 'scode did not exit');
    assert.equal(host.state.exitCode, 0);
  } finally { await host.close(); }
// The pinned Windows host can retain ConPTY handles after the child exits.
// Finish explicitly after assertions and host.close(), as the prose driver does.
})().then(() => process.exit(0)).catch(error => { console.error(error); process.exit(1); });
