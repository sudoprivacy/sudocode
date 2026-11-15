import { randomUUID } from 'node:crypto';
import * as fs from 'node:fs';
import * as path from 'node:path';

/**
 * Claude Code has one way to start a turn from outside the terminal without an
 * extra service: a background command hook with `asyncRewake: true` wakes the
 * session when it exits with code 2 and hands its stderr to Claude. That is the
 * Claude Code counterpart of Codex's `turn/start`.
 *
 * The bridge appends each admitted peer message to a private, append-only
 * spool. A hook waiter, armed on SessionStart and re-armed on every Stop, hands
 * every entry not yet delivered to Claude and exits 2. The waiter armed last is
 * the one that waits: Claude Code ends a waiter at its hook `timeout`, so the
 * newest one, armed at the end of the latest turn, covers the session longest.
 * A message that arrives while none is armed waits in the spool for the next.
 */
export interface ClaudeWakeEntry {
  at: string;
  from: string;
  verified: boolean;
  kind?: string;
  summary?: string;
  /** scode's `<mailbox-message>` block, the same text a scode model reads. */
  text: string;
}

export function appendClaudeWake(spool: string, entry: ClaudeWakeEntry): void {
  fs.appendFileSync(spool, JSON.stringify(entry) + '\n', { mode: 0o600 });
}

const attribute = (value: string) => value.replace(/&/g, '&amp;').replace(/"/g, '&quot;').replace(/</g, '&lt;');

/** The same `<channel>` frame a channel notification produces. */
export function formatClaudeWake(source: string, entry: ClaudeWakeEntry): string {
  const meta = [['from', entry.from], ['verified', String(entry.verified)],
    ...(entry.kind ? [['kind', entry.kind]] : []), ...(entry.summary ? [['summary', entry.summary]] : [])];
  return `<channel source="${attribute(source)}" ${meta.map(([key, value]) => `${key}="${attribute(value)}"`).join(' ')}>\n`
    + `${entry.text}\n</channel>`;
}

/** Claude Code enforces a hook's `timeout` on an `asyncRewake` hook (600 s
 * unless set) and ends the waiter there. The hook entry sets this value, and
 * the waiter returns on its own a minute earlier. */
export const CLAUDE_WAKE_HOOK_TIMEOUT_SECONDS = 86_400;

/** Replace `target` with `source` in one step. Windows refuses the replacement
 * while another waiter has the target open for a read, so retry briefly. */
function replace(source: string, target: string): void {
  for (let attempt = 0; ; attempt += 1) {
    try { fs.renameSync(source, target); return; } catch (error) {
      const code = (error as NodeJS.ErrnoException).code;
      if (attempt >= 50 || !['EPERM', 'EACCES', 'EBUSY'].includes(code ?? '')) {
        fs.rmSync(source, { force: true }); throw error;
      }
    }
    Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 20);
  }
}

function readOptional(file: string): string | undefined {
  try { return fs.readFileSync(file, 'utf8'); } catch (error) {
    if ((error as NodeJS.ErrnoException).code === 'ENOENT') return undefined;
    throw error;
  }
}

/** Make this waiter the spool's newest; an older waiter sees the change and returns. */
function arm(spool: string): string {
  const token = `${process.pid}-${randomUUID()}`;
  const temporary = `${spool}.waiter.${token}.tmp`;
  fs.writeFileSync(temporary, token, { flag: 'wx', mode: 0o600 });
  replace(temporary, spool + '.waiter');
  return token;
}

/**
 * Each delivered range of the spool is claimed once: `<spool>.delivered/<start>`
 * is created exclusively (a hard link of a complete file) and holds the range's
 * end. Two waiters that overlap during a handover therefore never deliver the
 * same entry, and a waiter ended at any instant leaves no partial claim.
 * `<spool>.cursor` only shortens the walk to the first unclaimed offset.
 */
function claim(spool: string, start: number, end: number): boolean {
  const claims = spool + '.delivered';
  const temporary = path.join(claims, `.${process.pid}-${randomUUID()}.tmp`);
  fs.writeFileSync(temporary, String(end), { flag: 'wx', mode: 0o600 });
  try { fs.linkSync(temporary, path.join(claims, String(start))); return true; } catch (error) {
    if ((error as NodeJS.ErrnoException).code === 'EEXIST') return false;
    throw error;
  } finally { fs.rmSync(temporary, { force: true }); }
}

/** The first offset no claim covers. */
function undelivered(spool: string): number {
  let offset = Number(readOptional(spool + '.cursor') ?? 0);
  if (!Number.isSafeInteger(offset) || offset < 0) throw new Error('The wake cursor is not an offset');
  for (;;) {
    const claimed = readOptional(path.join(spool + '.delivered', String(offset)));
    if (claimed === undefined) return offset;
    const end = Number(claimed);
    if (!Number.isSafeInteger(end) || end <= offset) throw new Error('A wake claim does not end after its start');
    offset = end;
  }
}

/** Complete lines after `offset`, and the offset after the last complete one. */
function pending(spool: string, offset: number): { entries: ClaudeWakeEntry[]; next: number } {
  const bytes = fs.existsSync(spool) ? fs.readFileSync(spool) : Buffer.alloc(0);
  if (offset > bytes.length) throw new Error('The wake spool is shorter than what was delivered from it; it was replaced');
  const end = bytes.lastIndexOf(0x0a) + 1;
  if (end <= offset) return { entries: [], next: offset };
  const entries = bytes.subarray(offset, end).toString('utf8').split('\n').filter(Boolean)
    .map(line => JSON.parse(line) as ClaudeWakeEntry);
  return { entries, next: end };
}

/** Claim every undelivered complete entry, or none when there is nothing new. */
function take(spool: string): ClaudeWakeEntry[] {
  for (;;) {
    const start = undelivered(spool);
    const { entries, next } = pending(spool, start);
    if (!entries.length) return [];
    if (claim(spool, start, next)) {
      // Only a shortcut: a failed update costs a longer walk, never a message.
      try {
        const temporary = `${spool}.cursor.${process.pid}-${randomUUID()}.tmp`;
        fs.writeFileSync(temporary, String(next), { flag: 'wx', mode: 0o600 });
        replace(temporary, spool + '.cursor');
      } catch { /* the claims stay authoritative */ }
      return entries;
    }
    // A waiter overlapping this one delivered that range; continue after it.
  }
}

export interface ClaudeWakeOptions {
  /** The MCP server name Claude Code knows the bridge by; it names the frame's source. */
  source?: string;
  pollMs?: number;
  /** Return quietly after this long; defaults to a minute before the hook timeout. */
  maxMs?: number;
  write?: (text: string) => void;
}

/** Wait for spooled peer messages; resolve the hook's exit code (2 wakes Claude). */
export async function waitForClaudeWake(spool: string, options: ClaudeWakeOptions = {}): Promise<number> {
  if (!path.isAbsolute(spool)) throw new Error('The wake spool must be an absolute path');
  fs.mkdirSync(spool + '.delivered', { recursive: true, mode: 0o700 });
  const token = arm(spool);
  const parent = process.ppid;
  const deadline = Date.now() + (options.maxMs ?? (CLAUDE_WAKE_HOOK_TIMEOUT_SECONDS - 60) * 1000);
  for (;;) {
    // A later turn armed a newer waiter, which now covers the session.
    if (readOptional(spool + '.waiter') !== token) return 0;
    // The session that armed this waiter is gone, or its hook timeout is near.
    if (process.ppid !== parent || Date.now() > deadline) return 0;
    const entries = take(spool);
    if (entries.length) {
      (options.write ?? (text => process.stderr.write(text)))(
        entries.map(entry => formatClaudeWake(options.source ?? 'nexus_scode', entry)).join('\n\n') + '\n');
      return 2;
    }
    await new Promise(resolve => setTimeout(resolve, options.pollMs ?? 500));
  }
}
