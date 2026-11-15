// Separate production controller process for the logged-before-send crash case.
// The parent owns the real native connection and stops this child at the first
// queue-add RPC boundary, after the controller has synced its submission slot.
const fs = require('node:fs');
const { openCodexMailboxWake } = require(process.argv[2]);
const config = JSON.parse(fs.readFileSync(process.argv[3], 'utf8'));
const pending = new Map();
let sequence = 0, wake;
process.on('message', message => {
  const waiter = pending.get(message.id);
  if (!waiter) return;
  pending.delete(message.id);
  if (message.error !== undefined) waiter.reject(new Error(message.error));
  else waiter.resolve(message.result);
});
process.on('disconnect', () => process.exit(1));
wake = openCodexMailboxWake({ ...config,
  qualified: async () => true, // Private fixture permission, never admission evidence.
  offer: async () => config.offer,
  native: { request(method, params) {
    return new Promise((resolve, reject) => {
      const id = ++sequence;
      pending.set(id, { resolve, reject });
      process.send({ id, method, params, status: wake.status() });
    });
  } },
});
wake.evaluate().then(() => {
  process.send({ unexpectedCompletion: wake.status() });
  process.exitCode = 1;
}).catch(error => { console.error(error); process.exitCode = 1; });
