// Additional prototype workload; the protected rendering gate stays unchanged.
// The shared host supplies the real OS PTY, terminal parser and failure traces.
const fs = require('node:fs');
const path = require('node:path');
const assert = require('node:assert/strict');
const { performance } = require('node:perf_hooks');
const { execFileSync } = require('node:child_process');
const shared = path.resolve(__dirname, '../../../../../e2e/terminal-resize');
const { createHost } = require(path.join(shared, 'host.cjs'));
const { resources } = require(path.join(shared, 'resources.cjs'));
const policy = require(path.join(shared, 'performance-policy.json'));
const config = JSON.parse(fs.readFileSync(process.argv[2], 'utf8'));
const host = createHost(config);
const { child, state, viewport, settle, sleep, close } = host;
const report = { version: 1, status: 'running', enabled: config.enabled,
  measurement: 'OS PTY dispatch to parsed xterm cells; headless terminal, no browser raster/GPU measurement.',
  metric_scope: 'cpu_ms and wire_kib sum the identical wait/stream/history typing phases. Total workload and additional scrolling costs remain explicit; baseline has no application history scrolling.',
  environment: host.metadata, samples: {}, metrics: { cpu_ms: 0, wire_kib: 0 }, resources: [] };

function resource(label) {
  let sample;
  if (process.platform === 'win32') {
    assert(Number.isSafeInteger(child.pid));
    const command = `$p=[Diagnostics.Process]::GetProcessById(${child.pid}); [pscustomobject]@{cpu_ms=$p.TotalProcessorTime.TotalMilliseconds;rss_mib=$p.WorkingSet64/1MB;peak_mib=$p.PeakWorkingSet64/1MB}|ConvertTo-Json -Compress`;
    sample = JSON.parse(execFileSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', command], { encoding: 'utf8', windowsHide: true }));
  } else sample = resources(child.pid);
  report.resources.push({ label, ...sample });
  return sample;
}

function parsed(predicate, action) {
  return new Promise((resolve, reject) => {
    let timer;
    const start = performance.now();
    const unsubscribe = host.onParsed(() => {
      if (!predicate()) return;
      clearTimeout(timer); unsubscribe(); resolve(performance.now() - start);
    });
    timer = setTimeout(() => { unsubscribe(); reject(new Error(`Missing parsed frame\n${viewport()}`)); }, 10000);
    action();
  });
}

const hasDraft = draft => viewport().split('\n').some(row => row.trimEnd() === `❯ ${draft}`.trimEnd());
const p95 = values => [...values].sort((a, b) => a - b)[Math.ceil(values.length * .95) - 1];
async function keys(phase) {
  const before = resource(`${phase}-start`), beforeBytes = state.bytes;
  let draft = '';
  const samples = report.samples[phase] = [];
  for (let i = 0; i < policy.sampling.input; i++) {
    if (i > 0 && i % 15 === 0) {
      await parsed(() => hasDraft(''), () => child.write('\x15'));
      draft = '';
    }
    draft += 'x';
    samples.push(await parsed(() => hasDraft(draft), () => child.write('x')));
    await sleep((i * 17) % 13);
  }
  await parsed(() => hasDraft(''), () => child.write('\x15'));
  report.metrics[`input_${phase}_p95_ms`] = p95(samples);
  const after = resource(`${phase}-end`);
  const cpu = after.cpu_ms - before.cpu_ms, wire = (state.bytes - beforeBytes) / 1024;
  report.metrics[`${phase}_cpu_ms`] = cpu;
  report.metrics[`${phase}_wire_kib`] = wire;
  if (phase !== 'reading') {
    report.metrics.cpu_ms += cpu;
    report.metrics.wire_kib += wire;
  }
}

(async () => {
  fs.mkdirSync(config.logRoot, { recursive: true });
  try {
    await settle(() => hasDraft(''));
    await sleep(100);
    const initial = resource('idle-start');
    // Process sampling is outside the timed idle and input observations.
    await settle(() => hasDraft(''));
    const idle = { bytes: state.bytes, frames: state.frames };
    await sleep(1000);
    report.contracts = { idle_bytes: state.bytes - idle.bytes, idle_frames: state.frames - idle.frames };
    assert.deepEqual(report.contracts, policy.contracts);
    await keys('wait');
    child.write(`${config.prompt}\r`);
    await host.wait(() => fs.existsSync(path.join(config.root, 'prose-ready')) && viewport().includes('ViewportRow089'));
    await keys('stream');
    const scrollStart = resource('scroll-start'), scrollBytes = state.bytes;
    const scroll = report.samples.scroll = [];
    for (let i = 0; i < 20; i++) {
      const input = i % 2 === 0 ? '\x1b[<64;20;5M' : '\x1b[<65;20;5M';
      if (config.enabled) {
        const before = viewport().split('\n').slice(1, 8).join('\n');
        scroll.push(await parsed(() => viewport().split('\n').slice(1, 8).join('\n') !== before,
          () => child.write(input)));
      } else { child.write(input); await sleep(8); }
    }
    if (config.enabled) {
      report.metrics.scroll_p95_ms = p95(scroll);
    }
    const scrollEnd = resource('scroll-end');
    report.metrics.scroll_cpu_ms = scrollEnd.cpu_ms - scrollStart.cpu_ms;
    report.metrics.scroll_wire_kib = (state.bytes - scrollBytes) / 1024;
    // Both modes receive the same input count; the opt-in view holds history.
    await keys('reading');
    child.write('\x1b[1;3F');
    await host.wait(() => viewport().includes('ViewportRow089'));
    fs.writeFileSync(path.join(config.root, 'prose-release'), 'release');
    await settle(text => text.includes('Σ') && !text.includes('Working...'));
    await keys('history');
    const after = resource('after-history');
    report.metrics.total_cpu_ms = after.cpu_ms - initial.cpu_ms;
    report.metrics.rss_peak_mib = after.peak_mib;
    report.metrics.rss_after_mib = after.rss_mib;
    report.metrics.rss_growth_mib = Math.max(0, after.rss_mib - initial.rss_mib);
    report.metrics.total_wire_kib = state.bytes / 1024;
    report.status = 'complete';
  } catch (error) {
    report.status = 'failed'; report.error = String(error.stack || error); process.exitCode = 1;
  } finally {
    fs.writeFileSync(config.report, JSON.stringify(report, null, 2));
    await close();
  }
})().then(() => process.exit(process.exitCode || 0)).catch(error => { console.error(error); process.exit(1); });
