const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
const { createHost } = require('./host.cjs');
const [python, backend, variant, writes] = process.argv.slice(2);
const name = `${backend}-${variant}-${writes}`;
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'scode-frame-replay-'));
const report = path.join(root, 'complete.json'), release = path.join(root, 'release');
const logRoot = path.resolve('frame-replay-results', name);
const host = createHost({ binary: python,
  args: [path.resolve('e2e/terminal-resize/prose-frame-replay.py'), path.resolve('e2e/terminal-resize/prose-frame-replay.json'), variant, writes, report, release],
  root, configHome: root, todos: path.join(root, 'todos.json'), logRoot,
  backend, cols: 120, rows: 40 });
(async () => {
  try {
    await host.settle(() => fs.existsSync(report));
    const lines = host.snapshot().split('\n');
    const start = lines.findIndex(line => line.includes('ProseStart'));
    let end = start;
    while (end >= 0 && end < lines.length - 1 && !lines[end].includes('ProseDone.')) end++;
    const rows = start >= 0 ? lines.slice(start, end + 1) : [];
    const joined = rows.join('').replace(/\s/g, '').normalize('NFC');
    const sourceExpected = 'ProseStart中文é💻ordinarytextwithboldwords,inlinecode,andalink(https://example.com).Moreordinaryprosewrapsacrossseveralrowswhiletheinputstaysavailable.ProseDone.'.normalize('NFC');
    const expected = variant === 'bmp_body' ? sourceExpected.replace('💻', 'ab') : sourceExpected;
    const viewportStart = start - host.terminal.buffer.active.baseY;
    const result = { name, start: viewportStart, rows, complete: joined.includes(expected),
      visible: viewportStart >= 0 && end - host.terminal.buffer.active.baseY < host.terminal.rows,
      starts: lines.filter(line => line.includes('ProseStart')).length,
      ...JSON.parse(fs.readFileSync(report, 'utf8')) };
    fs.mkdirSync(logRoot, { recursive: true });
    fs.writeFileSync(path.join(logRoot, 'result.json'), JSON.stringify(result, null, 2));
    console.log(JSON.stringify(result));
    fs.writeFileSync(release, 'release');
    for (let i = 0; i < 100 && !host.state.exited; i++) await host.sleep(20);
  } finally { await host.close(); }
})().then(() => process.exit(0)).catch(error => { console.error(error); process.exit(1); });
