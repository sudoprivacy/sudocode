// Prose preview acceptance through the shared real terminal observer.
const fs = require('node:fs');
const path = require('node:path');
const assert = require('node:assert/strict');

exports.run = async function run(host, config) {
  const { child, terminal, snapshot, viewport, settle, frame, resize, onPresentation } = host;
  const hasSavedHistory = ['prose_resize', 'prose_queued_resize', 'prose_finish_resize'].includes(config.scenario);
  const isCode = ['code_resize', 'code_queued_resize'].includes(config.scenario);
  const queuedResize = ['prose_queued_resize', 'code_queued_resize'].includes(config.scenario);
  const startMarker = isCode ? 'fn main()' : 'ProseStart';
  const endMarker = isCode ? 'PartialCodeTail' : 'ProseDone.';
function normalizedProse(text) {
  // This fixture's URI suffix depends on terminal capabilities. Dedicated
  // hyperlink acceptance checks the destination; retain all visible prose here.
  return text.replace(/\s/g, '').normalize('NFC')
    .replaceAll('link(https://example.com)', 'link');
}
function proseRows() {
  const lines = snapshot().split('\n');
  const start = lines.findIndex(line => line.includes(startMarker));
  let end = -1, body = '';
  for (let row = start; row >= 0 && row < lines.length; row++) {
    body += lines[row].replace(/\s/g, '');
    if (body.includes(endMarker)) { end = row; break; }
  }
  return start < 0 || end < start ? []
    : lines.slice(start, end + 1).map((text, offset) => {
      const row = start + offset, line = terminal.buffer.active.getLine(row), styles = [];
      for (let col = 0; col < terminal.cols; col++) {
        const cell = line.getCell(col);
        if (cell.getChars().trim()) styles.push([col, cell.getChars(), cell.getFgColorMode(),
          cell.getFgColor(), cell.getBgColorMode(), cell.getBgColor(), cell.isBold(),
          cell.isItalic(), cell.isUnderline()]);
      }
      return { row: row - terminal.buffer.active.baseY, text, styles };
    });
}
function checkProseHistory() {
  const text = snapshot();
  const joined = normalizedProse(text);
  assert.equal(joined.split(startMarker.replace(/\s/g, '')).length - 1, 1, text);
  assert.equal(joined.split(endMarker).length - 1, 1, text);
  if (hasSavedHistory) {
    for (let i = 0; i < 70; i++)
      assert.equal(text.split('\n').filter(line => line.trim() === `Earlier history line ${i}`).length,
        1, `history ${i} was lost or duplicated\n${text}`);
  }
}
assert.equal(typeof config.draft, 'string', 'the native fixture must provide its Unicode draft');
assert(config.draft.includes('👩🏽‍💻'), 'the native handoff must cover a non-BMP emoji sequence');
let draft = config.draft;
function inputAtWidth(cols) {
  const lines = viewport().split('\n');
  const input = lines.findIndex(line => line === `❯ ${draft}`);
  const separator = '─'.repeat(cols);
  return input > 0 && lines[input - 1] === separator && lines[input + 1] === separator;
}
async function presentInput(cols, edit) {
  // Parsed canvas cells can precede the synchronized frame's closing marker.
  // A completed older frame must not acknowledge the next draft edit.
  let isPresented = false;
  const stopWatching = onPresentation(() => { isPresented = inputAtWidth(cols); });
  try {
    edit();
    await frame(() => isPresented && inputAtWidth(cols));
  } finally { stopWatching(); }
}
async function resizeWithInput(cols, rows, suffix) {
  // ConPTY can repaint its old buffer before the application consumes Resize.
  // A fresh edit with adjacent separators proves the application presented the
  // new geometry; a wrapped old separator alone cannot acknowledge a resize.
  await presentInput(cols, () => {
    resize(cols, rows);
    draft += suffix;
    child.write(suffix);
  });
}
async function resizeWithQueuedFrame(cols, rows, suffix) {
  let isIntercepted = false;
  const listener = child.onData(data => {
    if (isIntercepted || !data.includes('\x1b[?2026h')) return;
    isIntercepted = true;
    // The shared observer has queued this old-size frame in xterm, whose
    // asynchronous parser has not consumed it yet. Reproduce a frontend
    // resize overtaking already emitted output, after every OS preflight.
    assert.equal(terminal.cols, 60);
    resize(cols, rows);
  });
  try {
    await presentInput(cols, () => {
      draft += suffix;
      child.write(suffix);
    });
    assert(isIntercepted, 'the old-size frame was not intercepted');
  } finally { listener.dispose(); }
}
async function prosePreview() {
  await settle(text => text.includes('❯'));
  child.write(config.prompt + '\r');
  await frame(() => viewport().replace(/\s/g, '').includes(endMarker)
    && fs.existsSync(path.join(config.root, 'prose-ready')));
  await presentInput(config.cols, () => child.write(draft));
  if (config.scenario === 'prose_finish_resize') {
    await resizeWithInput(40, 12, 't');
    // Finalization is deliberately released before the restored viewport has
    // presented, exercising history and live-frame placement in one update.
    resize(120, 40);
    fs.writeFileSync(path.join(config.root, 'prose-release'), 'release');
    const expected = normalizedProse('ProseStart中文é💻ordinarytextwithboldwords,inlinecode,andalink.Moreordinaryprosewrapsacrossseveralrowswhiletheinputstaysavailable.ProseDone.');
    await settle(() => viewport().includes('turn 2') && inputAtWidth(120)
      && normalizedProse(viewport()).includes(expected));
    checkProseHistory();
    child.write('\x15');
    await frame(() => !viewport().includes(draft));
    console.log('SCODE_XTERM_PROSE_PASS');
    return;
  }
  if (['prose_resize', 'prose_queued_resize'].includes(config.scenario) || isCode) {
    for (const [cols, rows, suffix] of [[60,40,'a'], [40,12,'b'], [120,40,'c'], [48,40,'d'], [120,40,'e']]) {
      if (queuedResize && cols === 40)
        await resizeWithQueuedFrame(cols, rows, suffix);
      else await resizeWithInput(cols, rows, suffix);
      // Preview may be clipped in a tiny window; durable earlier history must survive.
      if (hasSavedHistory) for (let i = 0; i < 70; i++)
        assert(snapshot().includes(`Earlier history line ${i}`), `lost history ${i}`);
    }
    await frame(text => text.replace(/\s/g, '').includes(endMarker));
  }
  const before = proseRows();
  assert(before.length > 0 && before.every(row => row.row >= 0 && row.row < terminal.rows),
    'the complete preview must be inside the current viewport');
  if (isCode) {
    const cells = marker => before.find(row => row.text.includes(marker)).styles;
    const string = cells('StreamString');
    const comment = cells('StreamComment');
    assert(string.some(cell => cell[2] !== 0), 'open string is already highlighted');
    assert(comment.some(cell => cell[2] !== 0), 'multiline comment parser state is retained');
    assert.notDeepEqual(string.map(cell => cell.slice(2, 4)),
      comment.map(cell => cell.slice(2, 4)), 'string and comment colors differ');
    assert(!snapshot().includes('```rust'), 'fence markup does not leak into source');
  }
  const handoffFrames = [];
  const stopWatching = onPresentation(() => handoffFrames.push(proseRows()));
  fs.writeFileSync(path.join(config.root, 'prose-release'), 'release');
  await settle(() => viewport().includes(`turn ${hasSavedHistory ? 2 : 1}`)
    && inputAtWidth(config.cols));
  stopWatching();
  const after = proseRows();
  // An unfinished code line stays plain until finalization. Its final syntax
  // color is allowed to change; all text/positions and complete-line styles
  // must stay identical in every presented handoff frame.
  const stableRows = rows => rows.map(row => isCode && row.text.includes(endMarker)
    ? { ...row, styles: [] } : row);
  assert.deepEqual(stableRows(after), stableRows(before), 'preview/history physical rows changed');
  assert(handoffFrames.length > 0, 'no synchronized handoff frames observed');
  for (const rows of handoffFrames) assert.deepEqual(stableRows(rows), stableRows(before), 'blank, duplicate or displaced handoff frame');
  if (isCode) {
    assert(before.find(row => row.text.includes(endMarker)).styles.every(cell => cell[2] === 0),
      'unfinished code line stays literal until finalization');
    assert(after.find(row => row.text.includes(endMarker)).styles.some(cell => cell[2] !== 0),
      'finalization applies canonical syntax colors to the unfinished line');
  }
  checkProseHistory();
  fs.writeFileSync(path.join(config.logRoot, 'handoff.json'), JSON.stringify({ before, after,
    frames: handoffFrames.length, cols: config.cols, scenario: config.scenario }, null, 2));
  child.write('\x15');
  await frame(() => !viewport().includes(draft));
  console.log('SCODE_XTERM_PROSE_PASS');
}
  await prosePreview();
};

// Functional acceptance reuses the pinned observer without modifying the
// protected performance workload runner or measurement fixtures.
if (require.main === module) {
  const { createHost } = require('../../../../../e2e/terminal-resize/host.cjs');
  const config = JSON.parse(fs.readFileSync(process.argv[2], 'utf8'));
  const host = createHost(config);
  (async () => {
    try {
      await exports.run(host, config);
      host.child.write('/exit');
      await host.settle(() => host.viewport().includes('❯ /exit'));
      host.child.write('\r');
      for (let i = 0; i < 250; i++) {
        if (host.state.exitCode === undefined && process.platform === 'win32')
          host.state.exitCode = host.child._agent?.exitCode;
        if (host.state.exited && host.state.exitCode !== undefined && !host.state.pending) break;
        await host.sleep(20);
      }
      assert(host.state.exited, 'scode did not exit');
      assert.equal(host.state.exitCode, 0);
    } finally {
      await host.close();
    }
  })().then(() => process.exit(0)).catch(error => { console.error(error); process.exit(1); });
}
