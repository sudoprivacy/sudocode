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
  const previous = fs.existsSync(journal) ? fs.readFileSync(journal, 'utf8').trim().split('\n').map(JSON.parse) : [];
  const holders = previous.filter(row => row.phase === 'holder');
  for (const { pid } of holders) {
    if (process.platform === 'win32') assert.throws(() => process.kill(pid, 0), { code: 'ESRCH' });
    else assert.ok(fs.existsSync(journal + '.drained'));
  }
  fs.appendFileSync(journal, JSON.stringify({ phase: 'proxy', pid: process.pid }) + '\n');
  if (previous.filter(row => row.phase === 'proxy').length === 1) {
    // Wait for each real descendant to start before the refused handshake.
    // On Windows the second one owns none of its parent's transport handles.
    for (const stderr of (process.platform === 'win32' ? ['inherit', 'ignore'] : ['inherit'])) {
      const holder = spawn(process.execPath, ['--input-type=module', '-e', `
        import fs from 'node:fs';
        const journal = process.argv[1];
        fs.appendFileSync(journal, JSON.stringify({phase:'holder',pid:process.pid}) + '\\n');
        process.send('ready');
        process.disconnect();
        setTimeout(() => fs.writeFileSync(journal + '.drained', 'drained'),
          process.platform === 'win32' ? 30000 : 2000);
      `, journal], { stdio: ['ignore', 'ignore', stderr, 'ipc'], detached: process.platform === 'win32', windowsHide: true });
      await new Promise((resolve, reject) => {
        holder.once('message', resolve);
        holder.once('error', reject);
        holder.once('exit', () => reject(new Error('fixture descendant exited before startup')));
      });
    }
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
