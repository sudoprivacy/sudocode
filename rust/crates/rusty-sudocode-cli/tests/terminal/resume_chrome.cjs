// Two real scode processes share one terminal, as when a user exits and resumes.
const fs = require('node:fs');
const path = require('node:path');
const assert = require('node:assert/strict');
const { createHost } = require('../../../../../e2e/terminal-resize/host.cjs');
const config = JSON.parse(process.argv[2]);
const windows = process.platform === 'win32';
const quote = value => windows ? "'" + value.replaceAll("'", "''") + "'"
  : "'" + value.replaceAll("'", "'\\''") + "'";
const launch = (windows ? '& ' : '') + [config.binary, ...config.args].map(quote).join(' ');
const boundary = 'SCODE_RESTART_READY';
const command = windows
  ? `${launch}\nif ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }\nWrite-Output '${boundary}'\n$null = [Console]::ReadLine()\n${launch}\nexit $LASTEXITCODE`
  : `${launch} || exit $?\nprintf '\\n${boundary}\\n'\nread -r restart\n${launch}`;
const shell = windows ? path.join(process.env.SystemRoot, 'System32/WindowsPowerShell/v1.0/powershell.exe') : '/bin/sh';
const host = createHost({ ...config, binary: shell,
  args: windows ? ['-NoLogo', '-NoProfile', '-Command', command] : ['-c', command] });
const { child, state, snapshot, viewport, settle, close } = host;
const count = marker => snapshot().split(marker).length - 1;
function history(copies, label) {
  const text = snapshot();
  for (let i = 0; i < 70; i++)
    assert.equal(text.split('\n').filter(row => row.trim() === `Earlier history line ${i}`).length,
      copies, `${label}: history ${i} was lost or duplicated\n${text}`);
  assert.equal(count('ResizeHistorySentinel'), copies, label);
}
function active(turn, copies) {
  history(copies, 'active');
  assert.equal(count(`\u00b7 turn ${turn} \u00b7`), 1, snapshot());
  assert.equal(count('\u00b7 turn '), 1, snapshot());
  assert.equal(count('11 todos'), 1, snapshot());
  for (const i of [7, 8, 9, 10])
    assert.equal(count(`TransientTodoSentinel${i}`), 1, snapshot());
}
function cleared(copies, label) {
  history(copies, label);
  for (const marker of ['\u00b7 turn ', '11 todos', 'TransientTodoSentinel', '\u23f5\u23f5'])
    assert.equal(count(marker), 0, `${label}: live chrome retained after exit\n${snapshot()}`);
}
async function exit() {
  child.write('/exit');
  await settle(() => viewport().includes('\u276f /exit'));
  child.write('\r');
}
(async () => {
  try {
    fs.mkdirSync(config.logRoot, { recursive: true });
    await settle(() => viewport().includes('turn 2') && viewport().includes('TransientTodoSentinel10'));
    active(2, 1);
    child.write(config.prompt + '\r');
    await settle(() => viewport().includes('turn 3') && snapshot().split('\n').some(row =>
      row.trim().replace(/^\u2022\s*/, '') === config.expectedReply));
    active(3, 1);
    await exit();
    await settle(() => viewport().includes(boundary));
    cleared(1, 'first exit');
    fs.writeFileSync(path.join(config.logRoot, 'between-processes.txt'), snapshot());
    child.write('\r');
    await settle(() => viewport().includes('turn 3') && viewport().includes('TransientTodoSentinel10'));
    active(3, 2);
    await exit();
    for (let i = 0; i < 250; i++) {
      if (state.exitCode === undefined && windows) state.exitCode = child._agent?.exitCode;
      if (state.exited && state.exitCode !== undefined && !state.pending) break;
      await host.sleep(20);
    }
    assert(state.exited, 'shell did not exit after both scode processes');
    assert.equal(state.exitCode, 0);
    cleared(2, 'second exit');
    console.log('SCODE_XTERM_EXIT_RESUME_CHROME_PASS');
  } finally {
    await close();
  }
})().then(() => process.exit(0)).catch(error => { console.error(error); process.exit(1); });
