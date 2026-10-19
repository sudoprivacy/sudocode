// Prose preview acceptance through the shared real terminal observer.
const fs = require('node:fs');
const path = require('node:path');
const assert = require('node:assert/strict');

exports.run = async function run(host, config) {
  const { child, terminal, snapshot, viewport, settle, frame, resize, onPresentation } = host;
  const hasSavedHistory = ['prose_resize', 'prose_finish_resize'].includes(config.scenario);
function proseRows() {
  const lines = snapshot().split('\n');
  const start = lines.findIndex(line => line.includes('ProseStart'));
  let end = -1, body = '';
  for (let row = start; row >= 0 && row < lines.length; row++) {
    body += lines[row].replace(/\s/g, '');
    if (body.includes('ProseDone.')) { end = row; break; }
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
  const joined = text.replace(/\s/g, '');
  assert.equal(joined.split('ProseStart').length - 1, 1, text);
  assert.equal(joined.split('ProseDone.').length - 1, 1, text);
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
  const input = lines.findIndex(line => line.startsWith(`❯ ${draft}`));
  const separator = '─'.repeat(cols);
  return input > 0 && lines[input - 1] === separator && lines[input + 1] === separator;
}
async function resizeWithInput(cols, rows, suffix) {
  // ConPTY can repaint its old buffer before the application consumes Resize.
  // A fresh edit with adjacent separators proves the application presented the
  // new geometry; a wrapped old separator alone cannot acknowledge a resize.
  const before = host.state.frames;
  resize(cols, rows);
  draft += suffix;
  child.write(suffix);
  await frame(() => host.state.frames > before && inputAtWidth(cols));
}
async function prosePreview() {
  await settle(text => text.includes('❯'));
  child.write(config.prompt + '\r');
  await frame(() => viewport().replace(/\s/g, '').includes('ProseDone.')
    && fs.existsSync(path.join(config.root, 'prose-ready')));
  child.write(draft);
  await frame(() => inputAtWidth(config.cols));
  if (config.scenario === 'prose_finish_resize') {
    await resizeWithInput(40, 12, 't');
    // Finalization is deliberately released before the restored viewport has
    // presented, exercising history and live-frame placement in one update.
    resize(120, 40);
    fs.writeFileSync(path.join(config.root, 'prose-release'), 'release');
    const expected = 'ProseStart中文é💻ordinarytextwithboldwords,inlinecode,andalink(https://example.com).Moreordinaryprosewrapsacrossseveralrowswhiletheinputstaysavailable.ProseDone.'.normalize('NFC');
    await settle(() => viewport().includes('turn 2') && inputAtWidth(120)
      && viewport().replace(/\s/g, '').normalize('NFC').includes(expected));
    checkProseHistory();
    child.write('\x15');
    await frame(() => !viewport().includes(draft));
    console.log('SCODE_XTERM_PROSE_PASS');
    return;
  }
  if (config.scenario === 'prose_resize') {
    for (const [cols, rows, suffix] of [[60,40,'a'], [40,12,'b'], [120,40,'c'], [48,40,'d'], [120,40,'e']]) {
      await resizeWithInput(cols, rows, suffix);
      // Preview may be clipped in a tiny window; durable earlier history must survive.
      for (let i = 0; i < 70; i++)
        assert(snapshot().includes(`Earlier history line ${i}`), `lost history ${i}`);
    }
    await frame(text => text.replace(/\s/g, '').includes('ProseDone.'));
  }
  const before = proseRows();
  assert(before.length > 0 && before.every(row => row.row >= 0 && row.row < terminal.rows),
    'the complete preview must be inside the current viewport');
  const handoffFrames = [];
  const stopWatching = onPresentation(() => handoffFrames.push(proseRows()));
  fs.writeFileSync(path.join(config.root, 'prose-release'), 'release');
  await settle(() => viewport().includes(`turn ${hasSavedHistory ? 2 : 1}`)
    && inputAtWidth(config.cols));
  stopWatching();
  const after = proseRows();
  assert.deepEqual(after, before, 'preview/history physical rows changed');
  assert(handoffFrames.length > 0, 'no synchronized handoff frames observed');
  for (const rows of handoffFrames) assert.deepEqual(rows, before, 'blank, duplicate or displaced handoff frame');
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
  const config = JSON.parse(process.argv[2]);
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
