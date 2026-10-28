// Both real CLI processes inherit this terminal; the parent only waits for Enter.
const fs = require('node:fs');
const path = require('node:path');
const assert = require('node:assert/strict');
const { spawnSync } = require('node:child_process');
const configPath = process.argv[2];
const config = JSON.parse(fs.readFileSync(configPath, 'utf8'));
const tracePath = path.join(path.dirname(configPath), 'launch.jsonl');
function trace(phase, fields = {}) {
  fs.appendFileSync(tracePath, JSON.stringify({ phase, time: Date.now(), pid: process.pid, ...fields }) + '\n');
}
function launch(index) {
  trace('start', { index });
  const result = spawnSync(config.binary, config.args, { stdio: 'inherit' });
  trace('exit', { index, status: result.status, signal: result.signal, error: result.error?.message });
  if (result.error) throw result.error;
  if (result.status !== 0) process.exit(result.status ?? 1);
}
assert(process.stdin.isTTY && process.stdout.isTTY && process.stderr.isTTY, 'launcher must inherit the native terminal');
trace('parent', { stdin: process.stdin.isTTY, stdout: process.stdout.isTTY, stderr: process.stderr.isTTY });
launch(1);
fs.writeSync(1, '\n' + config.boundary + '\n');
const byte = Buffer.alloc(1);
for (;;) {
  if (fs.readSync(0, byte, 0, 1, null) === 0) throw new Error('terminal closed before restart');
  if (byte[0] === 10) break;
}
trace('restart');
launch(2);
