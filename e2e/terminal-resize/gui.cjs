// Real browser terminal: OS PTY -> xterm DOM renderer -> raster screenshot.
// Mouse input goes through Chromium; never substitute terminal.select() for a drag.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
const { performance } = require('node:perf_hooks');
const { resources: processResource } = require('./resources.cjs');
const { createHash } = require('node:crypto');
const { chromium } = require('playwright');
const { WebSocketServer } = require('ws');
const { launch } = require('./host.cjs');

const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
const p95 = values => [...values].sort((a, b) => a - b)[Math.ceil(values.length * .95) - 1];
const hash = bytes => createHash('sha256').update(bytes).digest('hex');
// PNG readback ends near a compositor frame boundary. Sweep input dispatch
// across frame phases instead of phase-locking every key to the previous PNG.
// Both releases receive this same deterministic schedule outside timed regions.
const sampling = require('./gui-policy.json').sampling;
const inputDelaysMs = sampling.input_delays_ms;

async function run(config) {
  fs.mkdirSync(config.logRoot, { recursive: true });
  const report = { version: 1, status: 'running', metrics: {}, samples: {}, resources: [], clicks: [], input: [], input_timing: {}, wire_events: [],
    measurement: 'Frame latency: browser input dispatch to completed PNG capture after xterm render and two animation frames, including automation/readback. Parsed latency: an independent burst without screenshots, from browser keydown to onWriteParsed containing the expected draft, on the same browser clock. Neither measures physical display scanout.',
    resource_scope: 'scode process separately; summed RSS and CPU of dedicated Chromium process tree (shared RSS may be counted more than once); observer Node process excluded.' };
  const routes = {
    '/': ['text/html', path.join(__dirname, 'gui.html')],
    '/xterm.js': ['text/javascript', require.resolve('@xterm/xterm')],
    '/xterm.css': ['text/css', path.join(path.dirname(require.resolve('@xterm/xterm')), '../css/xterm.css')],
    '/unicode.js': ['text/javascript', require.resolve('@xterm/addon-unicode11')],
  };
  const server = http.createServer((req, res) => {
    const route = routes[new URL(req.url, 'http://localhost').pathname];
    if (!route) { res.writeHead(404).end(); return; }
    res.writeHead(200, { 'Content-Type': route[0] }); res.end(fs.readFileSync(route[1]));
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const websocket = new WebSocketServer({ server });
  let child, exitCode, browser, page, cdp, wireBytes = 0;
  const wire = [], processCpu = new Map();
  websocket.on('connection', socket => {
    child = launch({ ...config, env: { TERM_PROGRAM: 'vscode', TMUX: '', STY: '', ...config.env } }, { cols: config.cols, rows: config.rows });
    child.onData(data => {
      const bytes = Buffer.byteLength(data);
      report.wire_events.push({ at: performance.now(), offset: wireBytes, bytes });
      wireBytes += bytes; wire.push(data);
      if (socket.readyState === 1) socket.send(data);
    });
    child.onExit(event => { exitCode = event.exitCode; });
    socket.on('message', raw => {
      const message = JSON.parse(raw);
      if (typeof message.data === 'string') {
        report.input.push({ at: performance.now(), text: message.data.slice(0, 80), length: message.data.length });
        child.write(message.data);
      }
      else child.resize(message.cols, message.rows);
    });
  });
  try {
    browser = await chromium.launch({ headless: process.env.SCODE_GUI_HEADED !== '1' });
    cdp = await browser.newBrowserCDPSession();
    page = await browser.newPage({ viewport: { width: 1400, height: 1000 }, deviceScaleFactor: 1 });
    report.environment = { platform: process.platform, arch: process.arch, node: process.versions.node, cols: config.cols, rows: config.rows, deviceScaleFactor: 1, inputDelaysMs, sampling };
    report.browser = await browser.version();
    report.xterm = require('@xterm/xterm/package.json').version;
    await page.goto(`http://127.0.0.1:${server.address().port}/?options=${encodeURIComponent(JSON.stringify({ cols: config.cols, rows: config.rows }))}`);
    report.observer_time_origin = performance.timeOrigin;
    report.browser_time_origin = await page.evaluate(() => performance.timeOrigin);
    const wait = predicate => page.waitForFunction(predicate, null, { timeout: 30000, polling: 'raf' });
    await wait(() => window.fixture?.viewport().includes('❯'));
    await wait(() => fixture.lines().join('\n').includes('LinkBodyEND.'));
    // Replayed history can contain a prompt before the live input slot exists.
    // Start idle/interaction measurements only after a complete, quiet UI frame.
    // Contextual tips replace startup help; the permission prefix remains stable.
    await page.waitForFunction(cols => fixture.frames > 0 && !fixture.pending
      && fixture.viewport().split('\n').some(row => row === '─'.repeat(cols))
      && fixture.viewport().split('\n').some(row => /^\s*⏵⏵ danger-full-access(?:\s|$)/u.test(row))
      && performance.now() - fixture.lastData >= 300, config.cols);
    const readback = await page.context().newCDPSession(page);
    report.environment.pngReadback = { method: 'Page.captureScreenshot', optimizeForSpeed: true };
    report.paint_observations = [];
    const painted = async () => {
      const started = performance.now();
      const { clip, frameMs, boundsMs } = await page.evaluate(async () => {
        const began = performance.now();
        await document.fonts.ready;
        await fixture.frame();
        const framed = performance.now();
        if (document.getAnimations().some(animation => animation.playState === 'running')) {
          throw new Error('Terminal painting measurements require non-animated CSS');
        }
        const clip = fixture.pixelBounds();
        return { clip, frameMs: framed - began, boundsMs: performance.now() - framed };
      });
      assert(clip.width > 0 && clip.height > 0, 'missing terminal pixel bounds');
      const ready = performance.now();
      // Read the completed PNG directly. Generic page screenshots prepare
      // animation/caret state and repeat layout work before capturing pixels.
      const { data } = await readback.send('Page.captureScreenshot', {
        format: 'png', clip: { ...clip, scale: 1 },
        captureBeyondViewport: false, optimizeForSpeed: true,
      });
      report.paint_observations.push({ frame_ms: frameMs, bounds_ms: boundsMs,
        ready_rpc_ms: ready - started, readback_ms: performance.now() - ready,
        total_ms: performance.now() - started });
      return Buffer.from(data, 'base64');
    };
    const progress = label => fs.writeFileSync(path.join(config.logRoot, 'progress.txt'), label);
    const resources = async label => {
      progress(label);
      const processes = (await cdp.send('SystemInfo.getProcessInfo')).processInfo;
      const terminal = processes.map(p => {
        try { return { id: p.id, type: p.type, ...processResource(p.id), cpu_ms: p.cpuTime * 1000 }; }
        catch (error) { if (!fs.existsSync(`/proc/${p.id}`) && process.platform === 'linux') return null; throw error; }
      }).filter(Boolean);
      for (const p of terminal) processCpu.set(p.id, Math.max(processCpu.get(p.id) || 0, p.cpu_ms));
      const scode = processResource(child.pid);
      const value = { label, scode, terminal, terminal_cpu_ms: [...processCpu.values()].reduce((sum, cpu) => sum + cpu, 0),
        terminal_rss_mib: terminal.reduce((sum, p) => sum + p.rss_mib, 0) };
      report.resources.push(value); return value;
    };
    const baseline = await resources('ready');
    const idleBytes = wireBytes;
    await sleep(1000);
    assert.equal(wireBytes, idleBytes, 'idle terminal output');
    report.idle_bytes = wireBytes - idleBytes;
    async function reveal(label, last = false) {
      // Reflow can leave xterm's scrollbar and buffer offsets out of sync.
      // Navigation must expose the target before any hit position is computed;
      // never retry a click or relax its destination assertion.
      for (let attempt = 0; attempt < 3; attempt++) {
        const position = await page.evaluate(({ label, last }) => fixture.locate(label, last), { label, last });
        await page.evaluate(position => fixture.terminal.scrollToLine(Math.max(0, position.row - 2)), position);
        await painted();
        const visible = await page.evaluate(({ label, last }) => {
          const position = fixture.locate(label, last);
          const top = fixture.terminal.buffer.active.viewportY;
          return position.row >= top && position.row < top + fixture.terminal.rows ? position : null;
        }, { label, last });
        if (visible) return visible;
      }
      throw new Error(`Navigation did not expose ${label}`);
    }
    async function point(position, edge) { return page.evaluate(({ position, edge }) => fixture.point(position, edge), { position, edge }); }
    async function clickLink(label, target, last = false) {
      progress(`click:${label}`);
      const position = await reveal(label, last);
      const location = await point(position, 'start');
      report.clicks.push({ label, target, position, location, ...await page.evaluate(() => ({
        cols: fixture.terminal.cols, viewportY: fixture.terminal.buffer.active.viewportY, frames: fixture.frames,
      })) });
      await page.mouse.move(0, 0);
      const before = await page.evaluate(() => { fixture.hovered.length = 0; return fixture.activated.length; });
      await page.mouse.move(location.x + 2, location.y);
      await page.evaluate(() => fixture.frame());
      if (config.expectLinks && target) await page.waitForFunction(target => fixture.hovered.includes(target), target);
      await page.keyboard.down('Control');
      await page.mouse.click(location.x + 2, location.y);
      await page.keyboard.up('Control');
      if (config.expectLinks) assert.deepEqual(await page.evaluate(before => fixture.activated.slice(before), before), target ? [target] : [], `link target: ${label}`);
    }
    {
      await clickLink('PR #891', 'https://github.com/sudoprivacy/sudocode/pull/891');
      await clickLink('PlainAfter', null);
      await clickLink('LongDestination', `https://example.com/${'long-path-'.repeat(180)}`);
      await clickLink('QueryLink', 'https://example.com/search?q=a;b#fragment');
      await clickLink('SemicolonLink', `https://example.com/${'part;'.repeat(32)}tail`);
      await clickLink('中文👩🏽‍💻', 'https://example.com/table');
      await clickLink('ListEnd', 'https://example.com/list');
      await clickLink('DenseLink00', 'https://example.com/dense/0');
      const output = wire.join('');
      assert(!output.includes('\x1b]8;;javascript:'), 'unsafe destination emitted');
      assert(!output.includes('\x1b]8;;file:'), 'file destination promoted');
      assert(!output.includes(`\x1b]8;;https://example.com/${'x'.repeat(40)}`), 'oversized destination emitted');
    }
    async function drag(count) {
      const start = await reveal('DragStart');
      const end = await page.evaluate(() => fixture.locate('DragEnd'));
      const a = await point(start, 'start');
      const beforeDrag = await page.evaluate(() => fixture.activated.length);
      const dragSamples = [];
      await page.evaluate(() => fixture.terminal.clearSelection());
      await page.mouse.move(a.x, a.y); await page.mouse.down();
      const cols = await page.evaluate(() => fixture.terminal.cols);
      const distance = (end.row - start.row) * cols + end.end - start.start;
      let pixels = await painted();
      // Each sample is one pointer move -> highlighted frame, not total gesture duration.
      for (let i = 1; i <= count; i++) {
        const offset = start.start + Math.round(distance * i / count);
        const position = { row: start.row + Math.floor(offset / cols), end: offset % cols };
        const target = await point(i === count ? end : position, 'end');
        const began = performance.now();
        await page.mouse.move(target.x, target.y);
        const next = await painted();
        dragSamples.push(performance.now() - began);
        assert.notEqual(hash(pixels), hash(next), 'drag changed selection but no pixels');
        pixels = next;
      }
      await page.mouse.up();
      const selected = await page.evaluate(() => fixture.terminal.getSelection());
      assert(selected.startsWith('DragStart') && selected.endsWith('DragEnd'), `selection: ${selected}`);
      assert(selected.includes('👩🏽‍💻') && selected.includes('中文界') && selected.includes('é'), 'drag broke Unicode');
      assert(!selected.includes('\x1b') && !selected.includes('https://'), 'selection contains hidden metadata');
      fs.writeFileSync(path.join(config.logRoot, 'selection.png'), pixels);
      assert.equal(await page.evaluate(() => fixture.activated.length), beforeDrag, 'drag activated a link');
      return { selected, samples: dragSamples };
    }
    const selection = await drag(sampling.drag_steps);
    report.samples.drag_frame_ms = selection.samples;
    await page.evaluate(() => { fixture.terminal.clearSelection(); fixture.terminal.scrollToBottom(); fixture.terminal.focus(); });
    const streamWindows = inputDelaysMs.length * (sampling.parsed_bursts + sampling.painted_bursts) + 2;
    const streamDirectory = path.dirname(config.streamWindowPlan);
    fs.mkdirSync(streamDirectory, { recursive: true });
    fs.writeFileSync(config.streamWindowPlan, JSON.stringify({ windows: streamWindows }));
    report.stream_windows = [];
    let streamWindow = 0, sentChunks = 0;
    function releaseStreamWindow() {
      assert(streamWindow < streamWindows, 'more input windows than planned');
      const index = streamWindow++;
      fs.writeFileSync(path.join(streamDirectory, `permit-${index}`), 'release');
      return { index, released_at: performance.now(), wire_before: wireBytes };
    }
    async function finishStreamWindow(window) {
      const sent = path.join(streamDirectory, `sent-${window.index}`);
      const deadline = performance.now() + 30000;
      while (!fs.existsSync(sent)) {
        assert.equal(exitCode, undefined);
        assert(performance.now() < deadline, 'stream window did not send its deltas');
        await sleep(10);
      }
      const chunks = Number(fs.readFileSync(sent, 'utf8'));
      assert(Number.isSafeInteger(chunks) && chunks > sentChunks, 'stream window contained no deltas');
      sentChunks = chunks;
      report.stream_windows.push({ ...window, finished_at: performance.now(), chunks, wire_after: wireBytes });
    }
    async function keys(phase) {
      progress(`typing:${phase}`);
      await page.bringToFront();
      await page.locator('.xterm-helper-textarea').focus();
      await wait(() => document.hasFocus() && document.activeElement === fixture.terminal.textarea);
      const samples = [], parseSamples = [];
      // Readback can delay the next WebSocket message. Sample parsed input in
      // its own burst, then repeat the same keys with pixel evidence.
      for (const capture of [false, true]) {
        const timing = report.input_timing[`${phase}_${capture ? 'painted' : 'parsed'}`] = [];
        // Twenty observations leave P95 dependent on only two slow keys.
        // Repeat short drafts to sample the scheduler tail without wrapping
        // the input. Rotate the fixed delay sweep across successive bursts.
        const bursts = capture ? sampling.painted_bursts : sampling.parsed_bursts;
        for (let burst = 0; burst < bursts; burst++) {
          await page.evaluate(() => fixture.frame());
          let draft = '', previous = capture ? await painted() : null;
          for (let key = 0; key < inputDelaysMs.length; key++) {
            const delay = inputDelaysMs[(key + burst) % inputDelaysMs.length];
            draft += 'x';
            await page.evaluate(draft => fixture.armInput(draft), draft);
            const stream = phase === 'stream' ? releaseStreamWindow() : null;
            if (delay) await sleep(delay);
            const began = performance.now();
            await page.keyboard.type('x');
            const events = await page.evaluate(() => fixture.waitForInput());
            assert(events.sent >= events.keydown && events.parsed >= events.sent, 'input event ordering');
            timing.push({ ...events, burst, observed_ms: performance.now() - began });
            if (capture) {
              const next = await painted();
              samples.push(performance.now() - began);
              assert.notEqual(hash(previous), hash(next), 'input changed cells but no pixels');
              previous = next;
            } else {
              parseSamples.push(events.parsed - events.keydown);
            }
            // Outside the input timer: require real network progress before
            // another key, without racing screenshots against a finite EOF.
            if (stream) await finishStreamWindow(stream);
          }
          await page.keyboard.press('Control+u');
          await wait(() => fixture.viewport().split('\n').some(row => row.trimEnd() === '❯'));
        }
      }
      report.samples[`input_${phase}_frame_ms`] = samples;
      report.samples[`input_${phase}_parsed_ms`] = parseSamples;
    }
    await keys('history');
    await resources('history');
    child.write(config.prompt + '\r');
    const readyDeadline = performance.now() + 30000;
    while (!fs.existsSync(path.join(config.root, 'stream-ready'))) {
      assert.equal(exitCode, undefined);
      assert(performance.now() < readyDeadline, 'stream request did not start');
      await sleep(10);
    }
    await keys('wait');
    fs.writeFileSync(path.join(config.root, 'stream-release'), 'release');
    const duringStream = await drag(1);
    const headingsBefore = await page.evaluate(() => fixture.lines().join('\n').split('LinkHeading').length);
    const selectionWindow = releaseStreamWindow();
    await finishStreamWindow(selectionWindow);
    await page.waitForFunction(count => fixture.lines().join('\n').split('LinkHeading').length > count, headingsBefore);
    assert.equal(await page.evaluate(() => fixture.terminal.getSelection()), duringStream.selected, 'streaming changed the selected history');
    report.streaming_selection_preserved = true;
    await page.evaluate(() => { fixture.terminal.clearSelection(); fixture.terminal.scrollToBottom(); fixture.terminal.focus(); });
    await keys('stream');
    assert(!fs.existsSync(path.join(config.root, 'prose-ready')), 'stream completed before typing samples');
    await finishStreamWindow(releaseStreamWindow());
    assert.equal(streamWindow, streamWindows, 'stream/input window plan was not exhausted');
    fs.writeFileSync(path.join(config.root, 'prose-release'), 'release');
    await wait(() => fixture.viewport().includes('ctx ') && fixture.lines().join('\n').split('LinkBodyEND.').length >= 3);
    await resources('stream');
    // Model question descriptions use the same metadata through iocraft.
    child.write(`\x1b[200~${config.question}\x1b[201~`);
    await wait(() => fixture.viewport().includes('TOOL_BATCH:') || fixture.viewport().includes('Pasted'));
    child.write('\r');
    await wait(() => fixture.viewport().includes('[1] Continue') && fixture.viewport().includes('[2] Stop'));
    await painted();
    await clickLink('QuestionLink', 'https://example.com/question', true);
    await clickLink('then choose.', null, true);
    fs.writeFileSync(path.join(config.logRoot, 'question.png'), await painted());
    await page.keyboard.press('Escape');
    await wait(() => fixture.viewport().includes('⏹ Cancelled') && !fixture.viewport().includes('[1] Continue')
      && fixture.viewport().split('\n').some(row => row.trimEnd() === '❯'));
    // Resize and repaint while keeping link hit testing and native selection.
    for (const cols of [48, 110, 80]) {
      const before = await page.evaluate(cols => {
        const before = fixture.frames;
        fixture.resize(cols, 38);
        return before;
      }, cols);
      // Terminal reflow precedes the app's resize repaint. Match the shared
      // headless host's frame/width/quiet contract before resolving hit positions.
      await page.waitForFunction(({ cols, before }) => fixture.frames > before
        && fixture.viewport().split('\n').some(row => row === '─'.repeat(cols))
        && !fixture.pending && performance.now() - fixture.lastData >= 300, { cols, before });
      await painted();
      await clickLink('PR #891', 'https://github.com/sudoprivacy/sudocode/pull/891', true);
    }
    await page.evaluate(() => { fixture.terminal.scrollToBottom(); fixture.terminal.focus(); });
    const after = await resources('finished');
    for (const [key, values] of Object.entries(report.samples)) report.metrics[key.replace('_ms', '_p95_ms')] = p95(values);
    report.metrics.scode_cpu_ms = after.scode.cpu_ms - baseline.scode.cpu_ms;
    report.metrics.terminal_cpu_ms = after.terminal_cpu_ms - baseline.terminal_cpu_ms;
    report.metrics.scode_rss_peak_mib = Math.max(...report.resources.map(r => r.scode.rss_mib));
    report.metrics.terminal_rss_peak_mib = Math.max(...report.resources.map(r => r.terminal_rss_mib));
    report.metrics.scode_rss_growth_mib = Math.max(0, after.scode.rss_mib - baseline.scode.rss_mib);
    report.metrics.terminal_rss_growth_mib = Math.max(0, after.terminal_rss_mib - baseline.terminal_rss_mib);
    report.metrics.wire_kib = wireBytes / 1024;
    report.selected = selection.selected;
    report.status = 'passed';
    child.write('\x15');
    await wait(() => fixture.viewport().split('\n').some(row => row.trimEnd() === '❯'));
    await page.keyboard.type('/exit');
    await wait(() => fixture.viewport().includes('❯ /exit'));
    await page.keyboard.press('Enter');
    for (let i = 0; i < 100 && exitCode === undefined; i++) await sleep(20);
    assert.equal(exitCode, 0, 'normal CLI exit');
    console.log('SCODE_BROWSER_TERMINAL_PASS');
  } catch (error) {
    report.status = 'failed'; report.error = String(error.stack || error); throw error;
  } finally {
    if (page) {
      report.keyboard = await page.evaluate(() => window.fixture?.keyboard).catch(() => []);
      report.pending_input = await page.evaluate(() => window.fixture?.input).catch(() => null);
      fs.writeFileSync(path.join(config.logRoot, 'terminal.txt'), await page.evaluate(() => window.fixture?.lines().join('\n') || '').catch(() => ''));
      await page.screenshot({ path: path.join(config.logRoot, 'final.png') }).catch(() => {});
    }
    fs.writeFileSync(path.join(config.logRoot, 'wire.txt'), wire.join(''));
    fs.writeFileSync(config.report || path.join(config.logRoot, 'report.json'), JSON.stringify(report, null, 2));
    if (child && exitCode === undefined) child.kill();
    if (browser) await browser.close();
    for (const client of websocket.clients) client.terminate();
    websocket.close(); server.close();
  }
}

module.exports = { run };
