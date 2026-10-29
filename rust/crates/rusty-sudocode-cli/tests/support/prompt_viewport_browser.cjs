// Paint and interact with the real CLI through the existing browser terminal.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
const { performance } = require('node:perf_hooks');
const { createHash } = require('node:crypto');
const shared = path.resolve(__dirname, '../../../../../e2e/terminal-resize');
const moduleRoot = process.env.SCODE_VIEWPORT_BROWSER_MODULES || path.join(shared, 'node_modules');
const modulePath = name => path.join(moduleRoot, name);
const { chromium } = require(modulePath('playwright'));
const { WebSocketServer } = require(modulePath('ws'));
const { launch } = require(path.join(shared, 'host.cjs'));
const sampling = require(path.join(shared, 'performance-policy.json')).sampling;
const guiPolicy = require(path.join(shared, 'gui-policy.json'));
const { createBrowserSampler, processExited } = require(path.join(shared, 'resources.cjs'));
const config = JSON.parse(fs.readFileSync(process.argv[2], 'utf8'));
const routes = {
  '/': ['text/html', path.join(shared, 'gui.html')],
  '/xterm.js': ['text/javascript', require.resolve(modulePath('@xterm/xterm'))],
  '/xterm.css': ['text/css', path.join(path.dirname(require.resolve(modulePath('@xterm/xterm'))), '../css/xterm.css')],
  '/unicode.js': ['text/javascript', require.resolve(modulePath('@xterm/addon-unicode11'))],
};
const server = http.createServer((request, response) => {
  const route = routes[new URL(request.url, 'http://localhost').pathname];
  if (!route) { response.writeHead(404).end(); return; }
  response.writeHead(200, { 'Content-Type': route[0] }); response.end(fs.readFileSync(route[1]));
});
const websocket = new WebSocketServer({ server });
const report = { status: 'running', enabled: config.enabled, samples: [], frame_samples: [], checks: [], metrics: {}, resources: [],
  measurement: 'Real Chromium mouse/keyboard. Parsed keys use one browser clock; painted keys include dispatch, two animation frames and PNG readback. No physical display scanout measurement.',
  resource_scope: 'Dedicated browser process tree, including renderer/GPU; summed RSS can count shared pages more than once. Peak sums per-process high-water RSS where available, otherwise phase samples. Observer Node process excluded. Memory accounting requires Linux/macOS.' };
let child, browser, page, exited = false;
const wire = [];
websocket.on('connection', socket => {
  const terminalModules = process.env.SCODE_TERMINAL_MODULES || path.join(shared, 'node_modules');
  child = launch(config, { cols: config.cols, rows: config.rows }, name => require(path.join(terminalModules, name)));
  child.onData(data => { wire.push(data); if (socket.readyState === 1) socket.send(data); });
  child.onExit(() => { exited = true; });
  socket.on('message', raw => {
    const message = JSON.parse(raw);
    if (typeof message.data === 'string') child.write(message.data);
    else child.resize(message.cols, message.rows);
  });
});

(async () => {
  fs.mkdirSync(config.logRoot, { recursive: true });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  try {
    browser = await chromium.launch({ headless: true, executablePath: process.env.SCODE_VIEWPORT_CHROME || undefined });
    const cdp = await browser.newBrowserCDPSession();
    report.browser = await browser.version();
    const sampleBrowser = createBrowserSampler(cdp);
    async function resources(label) {
      const sample = await sampleBrowser(label);
      report.resources.push(sample);
      return sample;
    }
    page = await browser.newPage({ viewport: { width: 1300, height: 1000 } });
    await page.goto(`http://127.0.0.1:${server.address().port}/?options=${encodeURIComponent(JSON.stringify({ cols: config.cols, rows: config.rows }))}`);
    await page.waitForFunction(() => window.fixture?.viewport().includes('❯'));
    const initial = await resources('initial');
    await page.locator('.xterm-helper-textarea').focus();
    await page.keyboard.insertText(config.prompt);
    await page.keyboard.press('Enter');
    await page.waitForFunction(() => window.fixture.viewport().includes('ViewportRow089'));
    await page.keyboard.insertText('draft界');
    await page.waitForFunction(() => window.fixture.viewport().includes('draft界'));
    const rect = await page.locator('.xterm-screen').boundingBox();
    await page.mouse.move(rect.x + 300, rect.y + 120);
    await page.mouse.wheel(0, -350);
    await page.waitForFunction(enabled => {
      if (!enabled) { const buffer = window.fixture.terminal.buffer.active; return buffer.viewportY < buffer.baseY; }
      return !window.fixture.viewport().includes('ViewportRow089') && window.fixture.viewport().includes('ViewportRow');
    }, config.enabled);
    const visible = await page.evaluate(() => {
      const terminal = window.fixture.terminal, buffer = terminal.buffer.active;
      return Array.from({ length: terminal.rows }, (_, index) => buffer.getLine(buffer.viewportY + index).translateToString(true));
    });
    if (config.enabled) {
      assert(visible[0].includes('PROSE_CONTROL'), 'context belongs to the answer being read');
      assert(visible.slice(-6).some(row => row.includes('draft界')), 'composer remains fixed');
      report.checks.push('real wheel navigation; pinned prompt and draft');
    } else report.checks.push('real native terminal history scrolling');
    await page.screenshot({ path: path.join(config.logRoot, 'reading.png') });

    const label = visible.slice(1).map(row => row.match(/ViewportRow\d+/)?.[0]).find(Boolean);
    assert(label, 'visible answer for selection');
    const target = await page.evaluate(label => {
      const where = window.fixture.locate(label);
      return { start: window.fixture.point(where), end: window.fixture.point(where, 'end') };
    }, label);
    await page.evaluate(() => window.fixture.frame());
    // xterm uses Option on macOS and Shift elsewhere to override mouse
    // reporting. Native history needs no modifier; Shift extends a selection.
    const selectionModifier = await page.evaluate(() => {
      const mac = navigator.platform.startsWith('Mac');
      if (mac) window.fixture.terminal.options.macOptionClickForcesSelection = true;
      return mac ? 'Alt' : 'Shift';
    });
    report.selection_modifier = config.enabled ? selectionModifier : null;
    if (config.enabled) await page.keyboard.down(selectionModifier);
    await page.mouse.move(target.start.x, target.start.y);
    await page.mouse.down();
    await page.mouse.move(target.end.x, target.end.y, { steps: 12 });
    await page.mouse.up();
    if (config.enabled) await page.keyboard.up(selectionModifier);
    assert.equal(await page.evaluate(() => window.fixture.terminal.getSelection()), label);
    fs.writeFileSync(path.join(config.root, 'prose-release'), 'release');
    await page.waitForFunction(() => window.fixture.viewport().includes('Σ'));
    assert.equal(await page.evaluate(() => window.fixture.terminal.getSelection()), label, 'new output preserves selection');
    report.checks.push('native drag copies exact visible text and survives output');
    await resources('after-selection');

    await page.evaluate(() => window.fixture.resize(64, 36));
    await page.waitForFunction(() => window.fixture.terminal.cols === 64 && window.fixture.viewport().includes('draft界'));
    await page.screenshot({ path: path.join(config.logRoot, 'resized.png') });
    await page.locator('.xterm-helper-textarea').focus();
    await page.keyboard.press('Control+u');
    await page.waitForFunction(() => window.fixture.viewport().split('\n').some(row => row.trimEnd() === '❯'));
    for (let index = 0; index < sampling.input; index++) {
      const draft = 'x'.repeat(index + 1);
      await page.evaluate(draft => window.fixture.armInput(draft), draft);
      await page.keyboard.press('x');
      const input = await page.evaluate(() => window.fixture.waitForInput());
      report.samples.push(input.parsed - input.keydown);
    }
    report.parsed_key_p95_ms = [...report.samples].sort((a, b) => a - b)[Math.ceil(report.samples.length * .95) - 1];
    report.metrics.input_history_parsed_p95_ms = report.parsed_key_p95_ms;
    await page.keyboard.press('Control+u');
    await page.waitForFunction(() => window.fixture.viewport().split('\n').some(row => row.trimEnd() === '❯'));
    const hash = bytes => createHash('sha256').update(bytes).digest('hex');
    let previousPixels = hash(await page.screenshot());
    for (let index = 0; index < guiPolicy.sampling.input_delays_ms.length; index++) {
      await new Promise(resolve => setTimeout(resolve, guiPolicy.sampling.input_delays_ms[index]));
      await page.evaluate(draft => window.fixture.armInput(draft), 'x'.repeat(index + 1));
      const start = performance.now();
      await page.keyboard.press('x');
      await page.evaluate(() => window.fixture.waitForInput());
      await page.evaluate(() => window.fixture.frame());
      const pixels = hash(await page.screenshot());
      report.frame_samples.push(performance.now() - start);
      assert.notEqual(pixels, previousPixels, 'each painted key changes actual pixels');
      previousPixels = pixels;
    }
    report.metrics.input_history_frame_p95_ms = [...report.frame_samples].sort((a, b) => a - b)[Math.ceil(report.frame_samples.length * .95) - 1];
    await page.screenshot({ path: path.join(config.logRoot, 'typed.png') });
    const after = await resources('after-typing');
    report.metrics.terminal_cpu_ms = after.cpu_ms - initial.cpu_ms;
    if (after.rss_mib !== null) {
      report.metrics.terminal_rss_peak_mib = after.peak_mib;
      report.metrics.terminal_rss_growth_mib = Math.max(0, after.rss_mib - initial.rss_mib);
    }
    report.checks.push(`draft survives resize; ${sampling.input} real browser key events`);
    if (config.verifyProcessExit) {
      // Exercise an actual stale CDP snapshot after all measured work. This
      // browser is separate from the CLI's terminal, whose page stays alive.
      const spare = await chromium.launch({ headless: true, executablePath: process.env.SCODE_VIEWPORT_CHROME || undefined });
      try {
        const spareCdp = await spare.newBrowserCDPSession();
        await spare.newPage();
        const snapshot = (await spareCdp.send('SystemInfo.getProcessInfo')).processInfo;
        assert(snapshot.length > 0, 'real Chromium process snapshot');
        const staleSample = createBrowserSampler(spareCdp);
        await spare.close();
        for (let attempt = 0; attempt < 100 && !snapshot.every(entry => processExited(entry.id)); attempt++) {
          await new Promise(resolve => setTimeout(resolve, 20));
        }
        assert(snapshot.every(entry => processExited(entry.id)), 'owned browser processes have exited');
        const sampled = await staleSample('after-browser-exit', snapshot);
        assert.equal(sampled.processes.length, 0);
        assert.deepEqual(sampled.exited_processes.sort((a, b) => a - b), snapshot.map(entry => entry.id).sort((a, b) => a - b));
        assert.equal(sampled.cpu_ms, snapshot.reduce((sum, entry) => sum + entry.cpuTime * 1000, 0));
        assert(await page.evaluate(() => window.fixture.viewport().includes('❯')), 'main CLI terminal stays alive');
        report.resource_exit = { ...sampled, excluded_from_resource_window: true };
        report.checks.push('actual exited Chromium processes preserve CPU accounting without invalidating the terminal');
      } finally { await spare.close().catch(() => {}); }
    }
    report.status = 'complete';
  } catch (error) {
    report.status = 'failed'; report.error = String(error.stack || error); process.exitCode = 1;
  } finally {
    if (page) {
      report.final_text = await page.evaluate(() => window.fixture?.viewport()).catch(() => '');
      await page.screenshot({ path: path.join(config.logRoot, 'final.png') }).catch(() => {});
    }
    fs.writeFileSync(config.report, JSON.stringify(report, null, 2));
    fs.writeFileSync(path.join(config.logRoot, 'wire.txt'), wire.join(''));
    if (child && !exited) child.kill();
    if (browser) await browser.close();
    for (const client of websocket.clients) client.terminate();
    websocket.close(); server.close();
  }
})().then(() => process.exit(process.exitCode || 0)).catch(error => { console.error(error); process.exit(1); });
