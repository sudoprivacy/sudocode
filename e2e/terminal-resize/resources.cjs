// Process accounting shared by headless and painted-terminal measurements.
const fs = require('node:fs');
const assert = require('node:assert/strict');
const { execFileSync } = require('node:child_process');

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


module.exports = { resources };
