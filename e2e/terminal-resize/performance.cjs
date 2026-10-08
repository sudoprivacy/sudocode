// Workload assertions and measurements use the same host as resize acceptance.
const fs = require('node:fs');
const path = require('node:path');
const assert = require('node:assert/strict');
const { execFileSync } = require('node:child_process');
const { performance } = require('node:perf_hooks');

function percentile(values, fraction) {
  assert(values.length, 'no latency samples');
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[Math.ceil(sorted.length * fraction) - 1];
}

function resources(pid) {
  if (process.platform === 'linux') {
    // comm can contain spaces and parentheses; fields after its final ')' are fixed.
    const raw = fs.readFileSync(`/proc/${pid}/stat`, 'utf8');
    const stat = raw.slice(raw.lastIndexOf(')') + 2).trim().split(/\s+/);
    const status = fs.readFileSync(`/proc/${pid}/status`, 'utf8');
    const ticks = Number(execFileSync('getconf', ['CLK_TCK'], { encoding: 'utf8' }).trim());
    return { cpu_ms: (Number(stat[11]) + Number(stat[12])) * 1000 / ticks,
      rss_mib: Number(status.match(/^VmRSS:\s+(\d+)/m)?.[1]) / 1024,
      peak_mib: Number(status.match(/^VmHWM:\s+(\d+)/m)?.[1]) / 1024 };
  }
  assert.equal(process.platform, 'darwin', 'resource gate currently supports Linux/macOS');
  const fields = execFileSync('ps', ['-p', String(pid), '-o', 'rss=,time='], { encoding: 'utf8' }).trim().split(/\s+/);
  assert.equal(fields.length, 2, 'missing process accounting');
  const parts = fields[1].split(':').map(Number);
  const seconds = parts.reduce((total, value) => total * 60 + value, 0);
  return { cpu_ms: seconds * 1000, rss_mib: Number(fields[0]) / 1024,
    peak_mib: Number(fields[0]) / 1024 };
}

async function run(host, config) {
  const { child, state, wait, settle, viewport, snapshot, resizeFrame, sleep } = host;
  const report = { version: 1, case: config.case, spec: config.spec,
    preview: config.env.SUDOCODE_EXPERIMENT_PROSE_PREVIEW, metadata: host.metadata,
    metrics: {}, contracts: {}, input_samples_ms: {}, turns: [] };
  const samples = {}, turnCosts = [], resizeSamples = [], cancelSamples = [];
  let peak = 0;
  const sampleResource = () => {
    const resource = resources(child.pid);
    assert(Object.values(resource).every(Number.isFinite), 'invalid process accounting');
    peak = Math.max(peak, resource.peak_mib);
    return resource;
  };
  // Resource snapshots surround phases; no ps/getconf subprocesses run while
  // input latency is being measured. Linux supplies a kernel high-water mark.
  const fastWait = predicate => wait(predicate, { pollMs: 1, timeoutMs: 10000 });
  const hasDraft = draft => viewport().split('\n').some(row => row.trimEnd() === `❯ ${draft}`.trimEnd());
  async function clear() {
    child.write('\x15');
    await fastWait(() => hasDraft(''));
  }
  async function keys(phase) {
    let draft = '';
    const values = samples[phase] ||= [];
    for (let i = 0; i < config.sampling.input; i++) {
      draft += 'x';
      const started = performance.now();
      child.write('x');
      await fastWait(() => hasDraft(draft));
      values.push(performance.now() - started);
      await sleep(8);
    }
    await clear();
  }
  async function paste() {
    const draft = 'Draft界👩🏽‍💻';
    const started = performance.now();
    child.write(`\x1b[200~${draft}\x1b[201~`);
    await fastWait(() => hasDraft(draft));
    (samples.paste ||= []).push(performance.now() - started);
    return draft;
  }
  async function resized(draft) {
    for (let index = 0; index < config.sampling.resize; index++) {
      // Cover the refresh cycle instead of locking consecutive resizes to
      // whichever timer phase the first request happened to hit.
      await sleep((index * 17) % 83);
      const cols = index % 2 === 0 ? 80 : 120;
      const started = performance.now();
      await resizeFrame(cols, 50, () => hasDraft(draft), { pollMs: 1, timeoutMs: 10000 });
      resizeSamples.push(performance.now() - started);
    }
  }
  function file(name, value = 'release') { fs.writeFileSync(path.join(config.root, name), value); }
  function remove(name) { fs.rmSync(path.join(config.root, name), { force: true }); }
  const ready = name => fs.existsSync(path.join(config.root, name));
  try {
    await settle(text => text.includes('❯'));
    await sleep(100);
    const idle = { bytes: state.bytes, frames: state.frames, ...sampleResource() };
    await sleep(1000);
    const idleAfter = sampleResource();
    report.contracts.idle_bytes = state.bytes - idle.bytes;
    report.contracts.idle_frames = state.frames - idle.frames;
    report.metrics.idle_cpu_ms = idleAfter.cpu_ms - idle.cpu_ms;
    if (['background', 'foreground'].includes(config.spec.kind)) {
      const before = sampleResource(), beforeBytes = state.bytes;
      child.write(`\x1b[200~${config.prompt}\x1b[201~`);
      await fastWait(text => text.includes('Pasted') || text.includes('TOOL_BATCH:'));
      child.write('\r');
      if (config.spec.kind === 'background')
        await settle(text => text.includes('Concurrency batch done.') && text.includes('ctx '));
      else await wait(text => text.includes('╭─ Bash('));
      await keys('stream');
      if (config.spec.kind === 'background') {
        child.write('\x1b[B');
        await fastWait(() => viewport().includes('Enter to view'));
        child.write('\r');
        await fastWait(() => viewport().includes('Enter details'));
        child.write('\r');
        await fastWait(() => viewport().includes('Esc task list') && viewport().includes('PERFORMANCE_OUTPUT_LINE'));
        sampleResource();
        await sleep(1000);
        sampleResource();
        child.write('\x1b');
        await fastWait(() => viewport().includes('Enter details'));
        child.write('\x1b');
        await fastWait(() => hasDraft(''));
      }
      await wait(() => ready('producer-ready'));
      file('release');
      if (config.spec.kind === 'foreground')
        await settle(text => text.includes('Concurrency batch done.') && text.includes('ctx '));
      else {
        child.write('\x1b[B');
        await fastWait(() => viewport().includes('Enter to view'));
        child.write('\r');
        await fastWait(() => viewport().includes('completed'));
        child.write('\x1b');
        await settle(() => hasDraft(''));
      }
      await keys('history');
      const draft = await paste();
      await resized(draft);
      await clear();
      await settle(() => hasDraft(''));
      const after = sampleResource();
      turnCosts.push(after.cpu_ms - before.cpu_ms);
      report.metrics.wire_kib = (state.bytes - beforeBytes) / 1024;
      report.turns.push(after);
    } else {
      const expectedBody = JSON.parse(fs.readFileSync(path.join(config.root, 'prose-control.json'), 'utf8'))
        .document.replace(/\s/g, '');
      for (let turn = 1; turn <= config.spec.turns; turn++) {
        for (const name of ['prose-ready', 'prose-release', 'stream-ready', 'stream-release']) remove(name);
        const before = sampleResource(), beforeBytes = state.bytes;
        child.write(config.prompt + '\r');
        await wait(() => ready('stream-ready'));
        await keys('wait');
        let firstVisible;
        const started = performance.now();
        const stop = host.onParsed(() => {
          if (firstVisible === undefined && snapshot().replace(/\s/g, '').split('RenderBodySTART').length - 1 >= turn)
            firstVisible = performance.now() - started;
        });
        file('stream-release');
        await keys('stream');
        await wait(() => ready('prose-ready'));
        sampleResource();
        let draft = config.env.SUDOCODE_EXPERIMENT_PROSE_PREVIEW === '1' ? await paste() : '';
        file('prose-release');
        await settle(() => viewport().includes(`· turn ${turn} ·`) && hasDraft(draft));
        if (!draft) draft = await paste();
        stop();
        assert(Number.isFinite(firstVisible), 'first visible body was never observed');
        (samples.first_visible ||= []).push(firstVisible);
        const text = snapshot().replace(/\s/g, '');
        assert.equal(text.split('RenderBodySTART').length - 1, turn, 'lost/duplicated body start');
        assert.equal(text.split('RenderBodyEND.').length - 1, turn, 'lost/truncated/duplicated body end');
        if (config.spec.kind === 'prose')
          assert.equal(text.split(expectedBody).length - 1, turn, 'canonical body lost content');
        if (config.spec.kind === 'markdown')
          for (const marker of ['RenderHeading', 'CodeSentinel', 'TableSentinel', '中文界'])
            assert(text.includes(marker), `missing structured body ${marker}`);
        await resized(draft);
        await clear();
        const historyBefore = snapshot().replace(/\s/g, '').split('RenderBodyEND.').length;
        await keys('history');
        assert.equal(snapshot().replace(/\s/g, '').split('RenderBodyEND.').length,
          historyBefore, 'input edits changed durable body');
        const after = sampleResource();
        turnCosts.push(after.cpu_ms - before.cpu_ms);
        report.turns.push(after);
        (samples.wire ||= []).push((state.bytes - beforeBytes) / 1024);
      }
      // Each cancel has its own producer gates. Reusing a release path could
      // leave the previous SSE writer waiting after the next turn removes it.
      const controlPath = path.join(config.root, 'prose-control.json');
      const control = JSON.parse(fs.readFileSync(controlPath, 'utf8'));
      for (let index = 0; index < config.sampling.cancel; index++) {
        for (const key of ['start_ready', 'start_release', 'ready', 'release'])
          control[key] = path.join(config.root, `cancel-${index}-${key}`);
        fs.writeFileSync(controlPath, JSON.stringify(control));
        const previous = snapshot().toLowerCase().split('cancelled').length;
        child.write(config.prompt + '\r');
        await fastWait(() => fs.existsSync(control.start_ready) && viewport().includes('Thinking...'));
        const started = performance.now();
        child.write('\x1b');
        await fastWait(() => hasDraft('') && snapshot().toLowerCase().split('cancelled').length > previous
          && !viewport().includes('Thinking...'));
        cancelSamples.push(performance.now() - started);
        fs.writeFileSync(control.start_release, 'release');
        fs.writeFileSync(control.release, 'release');
      }
      report.metrics.cancel_ms = percentile(cancelSamples, 0.95);
      await settle(() => hasDraft(''));
      report.metrics.wire_kib = percentile(samples.wire, 0.5);
    }
    for (const [phase, values] of Object.entries(samples)) {
      if (['wait', 'stream', 'history'].includes(phase)) {
        report.metrics[`input_${phase}_p50_ms`] = percentile(values, 0.5);
        report.metrics[`input_${phase}_p95_ms`] = percentile(values, 0.95);
        report.input_samples_ms[phase] = values;
      }
    }
    report.metrics.paste_ms = percentile(samples.paste, 0.95);
    report.metrics.resize_ms = percentile(resizeSamples, 0.95);
    report.action_samples_ms = { resize: resizeSamples, cancel: cancelSamples, paste: samples.paste };
    if (samples.first_visible) report.metrics.first_visible_ms = percentile(samples.first_visible, 0.5);
    report.metrics.cpu_ms = percentile(turnCosts, 0.5);
    const after = sampleResource();
    report.metrics.rss_peak_mib = peak;
    report.after_actions = after;
    const lastTurn = report.turns.at(-1);
    report.metrics.rss_after_mib = lastTurn.rss_mib;
    report.metrics.rss_growth_mib = Math.max(0, lastTurn.rss_mib - report.turns[0].rss_mib);
    report.resource_method = process.platform === 'linux' ? 'proc process CPU / kernel VmHWM' : 'ps process CPU / sampled RSS peak';
    report.status = 'complete';
    console.log('SCODE_RENDER_PERFORMANCE_PASS');
  } catch (error) {
    report.status = 'failed'; report.error = String(error.stack || error);
    throw error;
  } finally {
    file('release'); file('stream-release'); file('prose-release');
    fs.mkdirSync(path.dirname(config.report), { recursive: true });
    fs.writeFileSync(config.report, JSON.stringify(report, null, 2));
  }
}

module.exports = { run };
