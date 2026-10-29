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


// A Chromium process can exit between CDP's snapshot and the OS read. Only
// ESRCH establishes that race; permission and malformed accounting errors fail.
function processExited(pid) {
  assert(Number.isSafeInteger(pid) && pid > 0, 'invalid process identity');
  try { process.kill(pid, 0); return false; }
  catch (error) { if (error.code === 'ESRCH') return true; throw error; }
}

function createBrowserSampler(cdp) {
  const cpu = new Map();
  let peak = 0;
  return async function sample(label, snapshot) {
    const processes = snapshot || (await cdp.send('SystemInfo.getProcessInfo')).processInfo;
    const exited = [];
    const rows = processes.map(entry => {
      // Consumed CPU remains accounted for after a renderer exits.
      cpu.set(entry.id, Math.max(cpu.get(entry.id) || 0, entry.cpuTime * 1000));
      if (process.platform === 'win32') return { ...entry, cpu_ms: entry.cpuTime * 1000 };
      try {
        const measured = resources(entry.id);
        return { ...entry, ...measured, os_cpu_ms: measured.cpu_ms, cpu_ms: entry.cpuTime * 1000 };
      }
      catch (error) {
        if (!processExited(entry.id)) throw error;
        exited.push(entry.id);
        return null;
      }
    }).filter(Boolean);
    const hasMemory = process.platform !== 'win32';
    const rss = hasMemory ? rows.reduce((total, row) => total + row.rss_mib, 0) : null;
    if (hasMemory) peak = Math.max(peak, rows.reduce((total, row) => total + row.peak_mib, 0));
    return { label, processes: rows, exited_processes: exited,
      cpu_ms: [...cpu.values()].reduce((sum, value) => sum + value, 0),
      rss_mib: rss, peak_mib: hasMemory ? peak : null };
  };
}

module.exports = { resources, processExited, createBrowserSampler };
