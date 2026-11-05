// Exact upstream table snapshots through the shared reflowing terminal.
const fs = require('node:fs');
const path = require('node:path');
const assert = require('node:assert/strict');

exports.run = async function run(host, config) {
  const { child, viewport, snapshot, wait, resizeFrame } = host;
  const options = { timeoutMs: config.timeoutMs };
  const draft = 'table-draft';
  const expected = config.case.expected.split('\n').map(row => row.trimEnd());
  const matchesSnapshot = () => {
    // Keep physical rows, the original two-column assistant prefix, and every
    // independent Codex snapshot byte. Only trailing terminal padding is ignored.
    const rows = snapshot().split('\n').map(row => [...row].slice(2).join('').trimEnd());
    return rows.some((_, start) => expected.every((row, offset) => rows[start + offset] === row));
  };
  await wait(() => viewport().split('\n').some(row => row.trimEnd() === '❯'), options);
  child.write(config.prompt + '\r');
  await wait(() => fs.existsSync(path.join(config.root, 'table-start-ready')), options);

  // The edit and completed frame acknowledge application geometry, rather
  // than merely ConPTY reflowing an old separator at the requested width.
  const resized = resizeFrame(config.case.width + 2, config.rows,
    () => viewport().split('\n').some(row => row.trimEnd() === `❯ ${draft}`), options);
  child.write(draft);
  await resized;
  fs.writeFileSync(path.join(config.root, 'table-start-release'), 'release');
  await wait(matchesSnapshot, options);
  assert(matchesSnapshot(), `exact upstream table snapshot ${config.case.name}`);
  await wait(() => fs.existsSync(path.join(config.root, 'prose-ready')), options);
  fs.writeFileSync(path.join(config.root, 'prose-release'), 'release');
  await resizeFrame(config.cols, config.rows,
    () => viewport().includes(`❯ ${draft}`) && snapshot().includes('· turn 1 ·'), options);
  assert(matchesSnapshot(), 'finalized table retains the exact narrow-width rows');
  child.write('\x15');
  await wait(() => viewport().split('\n').some(row => row.trimEnd() === '❯'), options);
  console.log(JSON.stringify({ snapshot: config.case.name, exact: true, input: true }));
};
