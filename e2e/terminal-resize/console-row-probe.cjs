const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
const { createHost } = require('./host.cjs');
const [python, backend, variant, mode] = process.argv.slice(2);
const name = `${backend}-${variant}-${mode}`;
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'scode-console-row-'));
const report = path.join(root, 'cursor.json'), release = path.join(root, 'release');
const logRoot = path.resolve('row-probe-results', name);
const host = createHost({ binary: python,
  args: [path.resolve('e2e/terminal-resize/console-row-probe.py'), variant, mode, report, release],
  root, configHome: root, todos: path.join(root, 'todos.json'), logRoot,
  backend, cols: 120, rows: 10 });
const expected = '• ProseStart 中文 é 💻 ordinary text with bold words, inline code, and a link (https://example.com). More ordinary prose';
(async () => {
  try {
    await host.settle(text => text.includes('ROW_PROBE_DONE') && fs.existsSync(report));
    const lines = host.viewport().split('\n');
    const result = { name, ...JSON.parse(fs.readFileSync(report, 'utf8')),
      firstLine: lines[0], expected, exact: lines[0] === expected,
      markerRow: lines.findIndex(line => line.includes('ROW_PROBE_DONE')) };
    fs.mkdirSync(logRoot, { recursive: true });
    fs.writeFileSync(path.join(logRoot, 'result.json'), JSON.stringify(result, null, 2));
    console.log(JSON.stringify(result));
    fs.writeFileSync(release, 'release');
    for (let i = 0; i < 100 && !host.state.exited; i++) await host.sleep(20);
  } finally { await host.close(); }
})().then(() => process.exit(0)).catch(error => { console.error(error); process.exit(1); });
