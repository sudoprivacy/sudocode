import { randomUUID } from 'node:crypto';
import * as fs from 'node:fs';
import * as path from 'node:path';
import type { ScodeMailboxHead } from './scodeMailbox';

/**
 * Claude Code takes input from outside the terminal through a command hook
 * with `asyncRewake: true`: when the hook exits with code 2, Claude Code hands
 * its stderr to Claude, mid-turn or by waking an idle session. That is the
 * Claude Code counterpart of the `thread/queue/add` a Codex attachment uses.
 *
 * The bridge appends each mailbox delivery to the session's private,
 * append-only spool. The `SessionStart` hook names the spool in `watchPaths`,
 * and Claude Code runs the `FileChanged` hook on each change to it. That hook
 * claims the complete entries no earlier run claimed and prints them.
 */
export interface ClaudeDeliveryEntry {
  /** `<hash of the budget key>:<n>`, the delivery's number in the bridge's log. */
  delivery: string;
  heads: ScodeMailboxHead[];
  /** The delivery provenance line, then scode's offer text. */
  text: string;
}

export function appendClaudeDelivery(spool: string, entry: ClaudeDeliveryEntry): void {
  fs.appendFileSync(spool, JSON.stringify(entry) + '\n', { mode: 0o600 });
}

/** Create `directory/name` holding `content`, or return false when it exists.
 * The name appears only as a hard link of a complete file, so a process ended
 * at any instant leaves no partial claim. */
export function createExclusive(directory: string, name: string, content: string): boolean {
  const temporary = path.join(directory, `.${process.pid}-${randomUUID()}.tmp`);
  fs.writeFileSync(temporary, content, { flag: 'wx', mode: 0o600 });
  try { fs.linkSync(temporary, path.join(directory, name)); return true; } catch (error) {
    if ((error as NodeJS.ErrnoException).code === 'EEXIST') return false;
    throw error;
  } finally { fs.rmSync(temporary, { force: true }); }
}

/** Replace `target` with `source` in one step. Windows refuses the replacement
 * while another hook run has the target open for a read, so retry briefly. */
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

/**
 * Each delivered range of the spool is claimed once: `<spool>.delivered/<start>`
 * is created exclusively and holds the range's end. Two hook runs for the same
 * change therefore never deliver the same entry.
 * `<spool>.cursor` only shortens the walk to the first unclaimed offset.
 */
const claim = (spool: string, start: number, end: number) => createExclusive(spool + '.delivered', String(start), String(end));

/** The first offset no claim covers. */
function undelivered(spool: string): number {
  let offset = Number(readOptional(spool + '.cursor') ?? 0);
  if (!Number.isSafeInteger(offset) || offset < 0) throw new Error('The delivery cursor is not an offset');
  for (;;) {
    const claimed = readOptional(path.join(spool + '.delivered', String(offset)));
    if (claimed === undefined) return offset;
    const end = Number(claimed);
    if (!Number.isSafeInteger(end) || end <= offset) throw new Error('A delivery claim does not end after its start');
    offset = end;
  }
}

/** Complete lines after `offset`, and the offset after the last complete one. */
function pending(spool: string, offset: number): { entries: ClaudeDeliveryEntry[]; next: number } {
  const bytes = fs.existsSync(spool) ? fs.readFileSync(spool) : Buffer.alloc(0);
  if (offset > bytes.length) throw new Error('The delivery spool is shorter than what was delivered from it; it was replaced');
  const end = bytes.lastIndexOf(0x0a) + 1;
  if (end <= offset) return { entries: [], next: offset };
  const entries = bytes.subarray(offset, end).toString('utf8').split('\n').filter(Boolean)
    .map(line => JSON.parse(line) as ClaudeDeliveryEntry);
  return { entries, next: end };
}

/** Claim every unclaimed complete entry and return their text for Claude, or
 * nothing when no entry is new. */
export function takeClaudeDeliveries(spool: string): string | undefined {
  if (!path.isAbsolute(spool)) throw new Error('The delivery spool must be an absolute path');
  fs.mkdirSync(spool + '.delivered', { recursive: true, mode: 0o700 });
  for (;;) {
    const start = undelivered(spool);
    const { entries, next } = pending(spool, start);
    if (!entries.length) return undefined;
    if (claim(spool, start, next)) {
      // Only a shortcut: a failed update costs a longer walk, never a delivery.
      try {
        const temporary = `${spool}.cursor.${process.pid}-${randomUUID()}.tmp`;
        fs.writeFileSync(temporary, String(next), { flag: 'wx', mode: 0o600 });
        replace(temporary, spool + '.cursor');
      } catch { /* the claims stay authoritative */ }
      return entries.map(entry => entry.text).join('\n\n') + '\n';
    }
    // A concurrent run claimed that range; continue after it.
  }
}
