import assert from 'node:assert/strict';
import net from 'node:net';
import fs from 'node:fs';
import { spawn } from 'node:child_process';

// Only a stdio proxy to the test's already-listening native protocol fixture.
const [port, scenario, ...args] = process.argv.slice(2);
assert.deepEqual(args, ['app-server', 'proxy', '--sock', 'fixture-native.sock']);
assert.ok(['normal', 'ignore-close', 'http-refusal', 'fail-reconnect-close'].includes(scenario));
assert.equal(process.env.SCODE_NATIVE_ENV_SENTINEL, 'owned-proxy');
assert.equal(process.env.CODEX_THREAD_ID, undefined, 'an explicit child environment is complete, not merged');
if (scenario === 'http-refusal') {
  process.stdout.write('HTTP/1.1 503 Fixture startup refusal\r\nContent-Length: 0\r\n\r\n', () => process.exit(23));
} else if (scenario === 'fail-reconnect-close') {
  const journal = process.env.SCODE_FIXTURE_RECONNECT_JOURNAL;
  assert.ok(journal);
  const replacement = fs.existsSync(journal);
  fs.appendFileSync(journal, JSON.stringify({ phase: 'proxy', pid: process.pid }) + '\n');
  if (replacement) {
    // A real child retains this proxy's stderr beyond the close deadline.
    // The owner cannot observe ChildProcess.close yet, even though the proxy
    // rejected its handshake. This is a bounded fixture, not a fake timer.
    const holder = spawn(process.execPath, ['-e',
      'setTimeout(() => { require("node:fs").writeFileSync(process.argv[1], "drained"); }, 2000);',
      journal + '.drained'], { stdio: ['ignore', 'ignore', 'inherit'], windowsHide: true });
    fs.appendFileSync(journal, JSON.stringify({ phase: 'stderr-holder', pid: holder.pid }) + '\n');
    process.stdout.write('HTTP/1.1 503 Fixture startup refusal\r\nContent-Length: 0\r\n\r\n', () => process.exit(23));
  } else {
    runProxy();
  }
} else {
  runProxy();
}

function runProxy() {
if (scenario === 'ignore-close') {
  process.on('SIGTERM', () => {});
  setInterval(() => {}, 1000);
}
const socket = net.createConnection({ host: '127.0.0.1', port: Number(port) });
process.stdin.pipe(socket);
socket.pipe(process.stdout);
socket.on('error', error => { console.error(error); process.exitCode = 1; });
socket.on('close', () => { if (scenario !== 'ignore-close') process.exit(); });
}
