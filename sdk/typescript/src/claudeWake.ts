import * as fs from 'node:fs';
import * as path from 'node:path';

/**
 * Claude Code has one way to start a turn from outside the terminal without an
 * extra service: a background command hook with `asyncRewake: true` wakes the
 * session when it exits with code 2 and hands its stderr to Claude. That is the
 * Claude Code counterpart of Codex's `turn/start`.
 *
 * The bridge appends each admitted peer message to a private spool. A hook
 * waiter, armed on SessionStart and re-armed on every Stop, hands everything
 * after its cursor to Claude and exits 2. One waiter runs per spool; a message
 * that arrives while none is armed waits in the spool for the next one.
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

function alive(pid: number): boolean {
  try { process.kill(pid, 0); return true; } catch (error) { return (error as NodeJS.ErrnoException).code === 'EPERM'; }
}

/** Claim the spool's single waiter slot; a dead owner's lock is reclaimed. */
function claim(lock: string): boolean {
  for (let attempt = 0; attempt < 2; attempt += 1) {
    try {
      fs.writeFileSync(lock, String(process.pid), { flag: 'wx', mode: 0o600 });
      return true;
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== 'EEXIST') throw error;
      const owner = Number(fs.readFileSync(lock, 'utf8'));
      if (Number.isInteger(owner) && owner > 0 && alive(owner)) return false;
      fs.rmSync(lock, { force: true });
    }
  }
  return false;
}

/** Complete lines after `offset`, and the offset after the last complete one. */
function pending(spool: string, offset: number): { entries: ClaudeWakeEntry[]; next: number } {
  let bytes: Buffer;
  try { bytes = fs.readFileSync(spool); } catch (error) {
    if ((error as NodeJS.ErrnoException).code === 'ENOENT') return { entries: [], next: offset };
    throw error;
  }
  const start = offset <= bytes.length ? offset : 0;
  const end = bytes.lastIndexOf(0x0a) + 1;
  if (end <= start) return { entries: [], next: start };
  const entries = bytes.subarray(start, end).toString('utf8').split('\n').filter(Boolean)
    .map(line => JSON.parse(line) as ClaudeWakeEntry);
  return { entries, next: end };
}

export interface ClaudeWakeOptions {
  /** The MCP server name Claude Code knows the bridge by; it names the frame's source. */
  source?: string;
  pollMs?: number;
  /** A waiter outliving its session exits quietly after this long. */
  maxMs?: number;
  write?: (text: string) => void;
}

/** Wait for spooled peer messages; resolve the hook's exit code (2 wakes Claude). */
export async function waitForClaudeWake(spool: string, options: ClaudeWakeOptions = {}): Promise<number> {
  if (!path.isAbsolute(spool)) throw new Error('The wake spool must be an absolute path');
  const lock = spool + '.wake.lock', cursorFile = spool + '.cursor';
  if (!claim(lock)) return 0;
  const parent = process.ppid;
  const deadline = Date.now() + (options.maxMs ?? 24 * 60 * 60_000);
  try {
    for (;;) {
      let cursor = 0;
      try { cursor = Number(fs.readFileSync(cursorFile, 'utf8')) || 0; } catch { /* first delivery */ }
      const { entries, next } = pending(spool, cursor);
      if (entries.length) {
        fs.writeFileSync(cursorFile, String(next), { mode: 0o600 });
        (options.write ?? (text => process.stderr.write(text)))(
          entries.map(entry => formatClaudeWake(options.source ?? 'nexus_scode', entry)).join('\n\n') + '\n');
        return 2;
      }
      // The session that armed this waiter is gone, or it has waited long enough.
      if (process.ppid !== parent || Date.now() > deadline) return 0;
      await new Promise(resolve => setTimeout(resolve, options.pollMs ?? 500));
    }
  } finally {
    fs.rmSync(lock, { force: true });
  }
}
