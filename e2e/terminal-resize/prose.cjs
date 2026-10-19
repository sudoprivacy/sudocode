// Prose preview acceptance through the shared real terminal observer.
const fs = require('node:fs');
const path = require('node:path');
const assert = require('node:assert/strict');

exports.run = async function run(host, config) {
  const { child, terminal, snapshot, viewport, settle, frame, resizeFrame, onPresentation } = host;
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
  if (config.scenario === 'prose_resize') {
    for (let i = 0; i < 70; i++)
      assert.equal(text.split('\n').filter(line => line.trim() === `Earlier history line ${i}`).length,
        1, `history ${i} was lost or duplicated\n${text}`);
  }
}
async function prosePreview() {
  await settle(text => text.includes('❯'));
  child.write(config.prompt + '\r');
  await frame(() => viewport().replace(/\s/g, '').includes('ProseDone.')
    && fs.existsSync(path.join(config.root, 'prose-ready')));
  child.write('DraftSurvivesProse');
  await frame(() => viewport().includes('❯ DraftSurvivesProse'));
  if (config.scenario === 'prose_resize') {
    for (const [cols, rows] of [[60,40], [40,12], [120,40], [48,40], [120,40]]) {
      await resizeFrame(cols, rows, () => viewport().includes('❯ DraftSurvivesProse'));
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
  await settle(() => viewport().includes(`turn ${config.scenario === 'prose_resize' ? 2 : 1}`)
    && viewport().includes('❯ DraftSurvivesProse'));
  stopWatching();
  const after = proseRows();
  assert.deepEqual(after, before, 'preview/history physical rows changed');
  assert(handoffFrames.length > 0, 'no synchronized handoff frames observed');
  for (const rows of handoffFrames) assert.deepEqual(rows, before, 'blank, duplicate or displaced handoff frame');
  checkProseHistory();
  fs.writeFileSync(path.join(config.logRoot, 'handoff.json'), JSON.stringify({ before, after,
    frames: handoffFrames.length, cols: config.cols, scenario: config.scenario }, null, 2));
  child.write('\x15');
  await frame(() => !viewport().includes('DraftSurvivesProse'));
  console.log('SCODE_XTERM_PROSE_PASS');
}
  await prosePreview();
};
