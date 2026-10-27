import assert from 'node:assert/strict';
import net from 'node:net';

// Only a stdio proxy to the test's already-listening native protocol fixture.
const [port, scenario, ...args] = process.argv.slice(2);
assert.deepEqual(args, ['app-server', 'proxy', '--sock', 'fixture-native.sock']);
assert.ok(['normal', 'ignore-close'].includes(scenario));
if (scenario === 'ignore-close') {
  process.on('SIGTERM', () => {});
  setInterval(() => {}, 1000);
}
const socket = net.createConnection({ host: '127.0.0.1', port: Number(port) });
process.stdin.pipe(socket);
socket.pipe(process.stdout);
socket.on('error', error => { console.error(error); process.exitCode = 1; });
socket.on('close', () => { if (scenario !== 'ignore-close') process.exit(); });
