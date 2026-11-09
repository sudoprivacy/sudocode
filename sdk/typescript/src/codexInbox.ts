import { createHash, randomUUID } from 'node:crypto';
import { closeSync, fsyncSync, lstatSync, mkdirSync, openSync, readFileSync, readdirSync,
  renameSync, unlinkSync, writeFileSync } from 'node:fs';
import * as path from 'node:path';
import type { ScodePeerMessage } from './scodeAcp';
import { CodexCleanupUnknownError, CodexRequestError } from './codexConnection';
import { codexPeerData } from './codexPeerData';

interface Record {
  version: 1; threadId: string; sessionId: string; eventId: string;
  messageHash: string; message?: ScodePeerMessage; receivedAt: number; sequence: number;
  state: 'pending' | 'done'; attempts: number;
}
const hash = (value: unknown) => createHash('sha256').update(JSON.stringify(value)).digest('hex');
const eventName = /^[a-f0-9-]{36}\.json$/;

/** A private durable handoff, separate from the diagnostic JSONL log. Only
 * idempotent downstream operations may be recovered from these pending rows.
 * Done receipts survive restart and upstream cursor replay. */
export function openCodexInbox(log: string, threadId: string, sessionId: string,
  deliver: (message: ScodePeerMessage, eventId: string) => Promise<unknown>,
  observe: (entry: object) => void): { receive(message: ScodePeerMessage): void; close(): Promise<void> } {
  const directory = log + '.inbox';
  mkdirSync(directory, { recursive: true, mode: 0o700 });
  if (!lstatSync(directory).isDirectory()) throw new Error('Callback inbox must be a private directory');
  if (process.platform !== 'win32') {
    // The log parent already exists. Persist the new inbox directory entry
    // before acknowledging any delivery stored inside it.
    const parent = openSync(path.dirname(directory), 'r');
    try { fsyncSync(parent); } finally { closeSync(parent); }
  }
  const pending = new Map<string, Record>();
  let lastSequence = 0;
  const sequences = new Set<number>();
  let closed = false, timer: ReturnType<typeof setTimeout> | undefined, active: Promise<void> | undefined;
  let cleanupError: CodexCleanupUnknownError | undefined;
  const report = (entry: object) => { try { observe(entry); } catch (error) { process.stderr.write(`Callback audit: ${String(error)}\n`); } };
  const file = (id: string) => path.join(directory, id + '.json');
  const save = (record: Record) => {
    const temporary = path.join(directory, '.' + randomUUID() + '.tmp');
    const fd = openSync(temporary, 'wx', 0o600);
    try {
      try { writeFileSync(fd, JSON.stringify(record) + '\n'); fsyncSync(fd); } finally { closeSync(fd); }
      renameSync(temporary, file(record.eventId));
      if (process.platform !== 'win32') {
        const parent = openSync(directory, 'r');
        try { fsyncSync(parent); } finally { closeSync(parent); }
      }
    } finally { try { unlinkSync(temporary); } catch (error) { if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error; } }
  };
  const read = (name: string): Record => {
    const target = path.join(directory, name);
    if (!lstatSync(target).isFile()) throw new Error('Callback receipt must be a regular private file');
    const record = JSON.parse(readFileSync(target, 'utf8')) as Record;
    if (record.version !== 1 || record.threadId !== threadId || record.sessionId !== sessionId
        || name !== record.eventId + '.json' || !['pending', 'done'].includes(record.state)
        || !Number.isSafeInteger(record.attempts) || record.attempts < 0
        || !Number.isSafeInteger(record.sequence) || record.sequence < 1
        || !Number.isFinite(record.receivedAt) || !/^[a-f0-9]{64}$/.test(record.messageHash)
        || (record.state === 'pending' && (!record.message || hash(record.message) !== record.messageHash))) {
      throw new Error('Callback inbox identity or content changed; refusing recovery');
    }
    return record;
  };
  for (const name of readdirSync(directory).filter(name => eventName.test(name))) {
    const record = read(name);
    if (sequences.has(record.sequence)) throw new Error('Callback inbox order changed; refusing recovery');
    sequences.add(record.sequence); lastSequence = Math.max(lastSequence, record.sequence);
    if (record.state === 'pending') pending.set(record.eventId, record);
  }
  const schedule = (delay = 0) => {
    if (closed || cleanupError || timer || active || !pending.size) return;
    timer = setTimeout(() => {
      timer = undefined;
      const record = [...pending.values()].sort((a, b) => a.sequence - b.sequence)[0];
      active = (async () => {
        try {
          // Persist before the RPC. On a crash, retry this SAME logical ID.
          record.attempts++; save(record);
          const result = await deliver(record.message!, record.eventId);
          const done: Record = { ...record, state: 'done' }; delete done.message;
          save(done); pending.delete(record.eventId);
          report({ eventId: record.eventId, phase: 'queued', ...(result as object), receipt: result });
        } catch (error) {
          if (error instanceof CodexCleanupUnknownError) cleanupError ??= error;
          report({ eventId: record.eventId, phase: 'pending', attempts: record.attempts, error: String(error), ...(error instanceof CodexRequestError ? {outcome: error.outcome} : {}) });
        }
      })().finally(() => {
        active = undefined;
        schedule(pending.has(record.eventId) ? Math.min(30000, 1000 * 2 ** Math.min(record.attempts - 1, 5)) : 0);
      });
    }, delay);
    timer.unref();
  };
  schedule();
  return {
    receive(message) {
      if (closed) throw new Error('Callback owner closed; upstream must retain delivery');
      if (Buffer.byteLength(JSON.stringify(message)) > 256 * 1024) throw new Error('Peer delivery exceeds durable inbox limit');
      const identity = message.deliveryId
        ? hash([sessionId, message.sudocode.a2a.from, message.deliveryId]).slice(0, 32)
        : randomUUID().replaceAll('-', '');
      const eventId = `${identity.slice(0,8)}-${identity.slice(8,12)}-${identity.slice(12,16)}-${identity.slice(16,20)}-${identity.slice(20)}`;
      // JSON escaping can expand a small source message beyond the producer's
      // wire limit. Refuse before ACK rather than persist an undeliverable head.
      if (Buffer.byteLength(JSON.stringify(codexPeerData(message, eventId).text)) > 256 * 1024)
        throw new Error('Encoded peer delivery exceeds native queue limit');
      const messageHash = hash(message);
      try {
        const previous = read(eventId + '.json');
        if (previous.messageHash !== messageHash) throw new Error('Delivery identity reused with different bytes');
        // A previous attempt may have renamed successfully but failed its
        // directory fsync. Existence alone must never become a durable ACK.
        save(previous);
        if (previous.state === 'pending') pending.set(eventId, previous);
        schedule(); return;
      } catch (error) { if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error; }
      if (pending.size >= 64) throw new Error('Callback inbox full; upstream must retain delivery');
      if (!Number.isSafeInteger(lastSequence + 1)) throw new Error('Callback inbox sequence exhausted');
      // Copy the accepted bytes; the caller cannot mutate an already-ACKed job.
      const record: Record = { version: 1, threadId, sessionId, eventId, messageHash,
        message: JSON.parse(JSON.stringify(message)), state: 'pending', receivedAt: Date.now(), sequence: ++lastSequence, attempts: 0 };
      save(record); pending.set(eventId, record);
      report({ eventId, phase: 'received', message }); schedule();
    },
    async close() {
      closed = true; clearTimeout(timer); await active;
      // The owner must retain custody when an old proxy's exit is unknown.
      // The delivery catch keeps pending bytes; it must not turn UNKNOWN into
      // successful cleanup merely because the asynchronous attempt settled.
      if (cleanupError) throw cleanupError;
    },
  };
}
