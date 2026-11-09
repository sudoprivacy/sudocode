import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { createServer } from 'node:net';
import * as path from 'node:path';
import { windowsProcessOwnerSource } from './windowsProcessOwnerSource';

/** Stored once in the same private MCP config as the existing SID/callback.
 * FILETIME is a decimal string: JS numbers cannot represent every 100ns tick.
 * This is process lifetime identity, not an approval or message-delivery grant.
 */
export interface ReceiverOwnerIdentity {
  platform: 'win32';
  pid: number;
  createdFiletime100ns: string;
  exe: string;
}

export type ReceiverOwnerEnd =
  | { reason: 'owner_exited' | 'receiver_exited' | 'channel_closed' }
  | { reason: 'monitor_failed'; detail: string };

export interface ReceiverOwnerLease {
  /** Returned child PID for diagnostics, never a later process lookup. */
  monitorPid: number;
  ended: Promise<ReceiverOwnerEnd>;
  close(): Promise<void>;
}

export class ReceiverOwnerCleanupUnknownError extends Error {}

export function validateReceiverOwner(value: ReceiverOwnerIdentity): void {
  if (!value || Object.keys(value).sort().join(',') !== 'createdFiletime100ns,exe,pid,platform'
      || value.platform !== 'win32' || !Number.isSafeInteger(value.pid) || value.pid <= 0
      || typeof value.createdFiletime100ns !== 'string' || !/^[1-9][0-9]{0,19}$/.test(value.createdFiletime100ns)
      || typeof value.exe !== 'string' || !path.win32.isAbsolute(value.exe) || /[\0\r\n]/.test(value.exe)) {
    throw new Error('receiverOwner requires one exact Windows process identity');
  }
}

function windowsPowerShell(): string {
  const root = process.env.SystemRoot;
  if (process.platform !== 'win32' || !root || !path.isAbsolute(root)) {
    throw new Error('Windows process ownership requires Windows; no PID-polling fallback');
  }
  return path.join(root, 'System32', 'WindowsPowerShell', 'v1.0', 'powershell.exe');
}

function startAdapter(request: Record<string, unknown>, signal?: AbortSignal) {
  signal?.throwIfAborted();
  const child = spawn(windowsPowerShell(), ['-NoLogo', '-NoProfile', '-NonInteractive', '-EncodedCommand',
    Buffer.from(windowsProcessOwnerSource, 'utf16le').toString('base64')],
  { windowsHide: true, shell: false, stdio: ['pipe', 'pipe', 'pipe'] });
  let buffer = '', stderr = '', settled = false, closing = false, bootstrapComplete = false;
  let finish!: (result: ReceiverOwnerEnd) => void;
  const ended = new Promise<ReceiverOwnerEnd>(resolve => { finish = resolve; });
  const end = (result: ReceiverOwnerEnd) => { if (!settled) { settled = true; finish(result); } };
  let ready!: (identity: ReceiverOwnerIdentity) => void, refuse!: (error: Error) => void;
  const initialized = new Promise<ReceiverOwnerIdentity>((resolve, reject) => { ready = resolve; refuse = reject; });
  // Initial qualification can refuse; it is never used as a recurring liveness
  // timer. After readiness the OS wait, not elapsed wall time, decides exit.
  const startup = setTimeout(() => refuse(new Error('Receiver owner qualification timed out; liveness UNKNOWN')), 10000);
  const onAbort = () => { refuse(new Error('Receiver owner qualification cancelled')); child.stdin.end(); };
  signal?.addEventListener('abort', onAbort, { once: true });
  if (signal?.aborted) onAbort();
  const closed = new Promise<void>(resolve => child.once('close', (code, exitSignal) => {
    clearTimeout(startup); signal?.removeEventListener('abort', onAbort);
    refuse(new Error(`Receiver owner monitor closed before qualification: ${code}/${exitSignal}; ${stderr}`));
    end(closing ? { reason: 'channel_closed' } : {
      reason: 'monitor_failed', detail: `Owner liveness UNKNOWN: monitor closed ${code}/${exitSignal}; ${stderr}`,
    });
    resolve();
  }));
  child.on('error', error => { refuse(error); end({ reason: 'monitor_failed', detail: String(error) }); });
  child.stdin.on('error', error => { if (!closing) { refuse(error); end({ reason: 'monitor_failed', detail: String(error) }); } });
  child.stderr.setEncoding('utf8');
  child.stderr.on('data', (text: string) => { stderr = (stderr + text).slice(-4096); });
  child.stdout.setEncoding('utf8');
  child.stdout.on('data', (text: string) => {
    buffer += text;
    try {
      if (Buffer.byteLength(buffer) > 16384) throw new Error('Owner monitor output exceeds bound');
      let newline: number;
      while ((newline = buffer.indexOf('\n')) >= 0) {
        const line = buffer.slice(0, newline).trim(); buffer = buffer.slice(newline + 1);
        if (!line) continue;
        const value = JSON.parse(line);
        if (value.phase === 'bootstrap_complete' && !bootstrapComplete) {
          bootstrapComplete = true; continue;
        }
        if (value.phase === 'captured' || value.phase === 'ready') {
          validateReceiverOwner(value.identity);
          if (value.identity.pid !== request.pid
              || (request.mode === 'watch' && (value.identity.exe !== request.exe
                || value.identity.createdFiletime100ns !== request.createdFiletime100ns))
              || value.phase !== (request.mode === 'capture' ? 'captured' : 'ready')) {
            throw new Error('Owner monitor returned a different identity or operation');
          }
          if (!bootstrapComplete) throw new Error('Owner identity arrived before compiler bootstrap completed');
          clearTimeout(startup); ready(Object.freeze(value.identity));
        } else if (value.phase === 'ended' && ['owner_exited', 'receiver_exited', 'channel_closed'].includes(value.reason)) {
          end({ reason: value.reason });
        } else throw new Error(`Owner monitor refused: ${value.error ?? 'invalid protocol'}`);
      }
    } catch (error) { refuse(error as Error); end({ reason: 'monitor_failed', detail: String(error) }); }
  });
  child.stdin.write(JSON.stringify(request) + '\n');
  let closingTask: Promise<void> | undefined;
  const close = () => closingTask ??= (async () => {
    closing = true; clearTimeout(startup); child.stdin.end();
    const wait = async (ms: number) => {
      let timer: NodeJS.Timeout | undefined;
      try { return await Promise.race([closed.then(() => true), new Promise<false>(resolve => { timer = setTimeout(() => resolve(false), ms); })]); }
      finally { clearTimeout(timer); }
    };
    if (!await wait(2000)) {
      // Only our returned helper handle may be terminated. Never signal the
      // selected owner, a PID discovered later, an original server or a peer.
      child.kill();
      if (!await wait(2000)) throw new ReceiverOwnerCleanupUnknownError('Owned monitor exit unobserved; no replacement receiver permitted');
    }
    // Windows PowerShell may invoke its compiler while Add-Type initializes.
    // Its explicit completion marker distinguishes an identity refusal from
    // termination during compilation, including unexpected external exit.
    if (child.pid !== undefined && !bootstrapComplete) throw new ReceiverOwnerCleanupUnknownError('Monitor bootstrap interrupted; descendant cleanup UNKNOWN; no replacement receiver permitted');
  })();
  return { initialized, ended, close, pid: child.pid };
}

/** Read-only preparation. A later watcher must re-match this exact object;
 * capturing a replacement is never automatic recovery.
 */
export async function captureReceiverOwner(pid: number): Promise<ReceiverOwnerIdentity> {
  if (!Number.isSafeInteger(pid) || pid <= 0) throw new Error('Owner PID must be a positive integer');
  const adapter = startAdapter({ mode: 'capture', pid });
  try { return await adapter.initialized; }
  finally { await adapter.close(); }
}

/** One receiver per original process and ACP session, with OS-owned locking.
 * A copied/renamed config cannot create a second receiver for the same owner.
 * No persisted PID/heartbeat is used as a second liveness authority.
 */
export async function acquireReceiverOwner(identity: ReceiverOwnerIdentity, acpSessionId: string,
  signal?: AbortSignal): Promise<ReceiverOwnerLease> {
  validateReceiverOwner(identity); windowsPowerShell(); signal?.throwIfAborted();
  if (typeof acpSessionId !== 'string' || !acpSessionId.trim() || /[\0\r\n]/.test(acpSessionId)) {
    throw new Error('Receiver ownership requires the existing ACP session ID');
  }
  const key = createHash('sha256').update(JSON.stringify([
    identity.pid, identity.createdFiletime100ns, identity.exe.toLowerCase(), acpSessionId,
  ])).digest('hex');
  const lock = createServer(socket => socket.destroy());
  await new Promise<void>((resolve, reject) => {
    lock.once('error', reject);
    lock.listen(`\\\\.\\pipe\\scode-receiver-owner-${key}`, () => { lock.removeListener('error', reject); resolve(); });
  });
  const unlock = () => new Promise<void>((resolve, reject) => lock.close(error => error ? reject(error) : resolve()));
  let adapter: ReturnType<typeof startAdapter> | undefined;
  try {
    adapter = startAdapter({ mode: 'watch', ...identity, receiverPid: process.pid }, signal);
    await adapter.initialized;
    if (adapter.pid === undefined) throw new Error('Qualified monitor has no returned PID');
    let closing: Promise<void> | undefined;
    const lease = adapter;
    return { monitorPid: adapter.pid, ended: lease.ended,
      close: () => closing ??= (async () => { await lease.close(); await unlock(); })() };
  } catch (error) {
    await adapter?.close(); // If cleanup is unknown, retain the exclusive lock.
    await unlock();
    throw error;
  }
}
