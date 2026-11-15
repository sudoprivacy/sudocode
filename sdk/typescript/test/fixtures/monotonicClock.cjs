// Test preload: this process's monotonic clock (performance.now) runs ahead by
// the milliseconds last written to the file named by SCODE_FIXTURE_CLOCK.
// Wall-clock time is unchanged.
const { readFileSync } = require('node:fs');
const { performance } = require('node:perf_hooks');
const file = process.env.SCODE_FIXTURE_CLOCK;
const now = performance.now.bind(performance);
Object.defineProperty(performance, 'now', { configurable: true, writable: true,
  value: () => now() + Number(readFileSync(file, 'utf8') || 0) });
