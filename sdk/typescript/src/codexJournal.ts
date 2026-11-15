import { createHash } from 'node:crypto';
import { closeSync, constants, fstatSync, fsyncSync, lstatSync, openSync, readFileSync, writeFileSync } from 'node:fs';
import * as path from 'node:path';

export const journalHash = (value: unknown): string => createHash('sha256').update(JSON.stringify(value)).digest('hex');
const writers = new Set<string>();

/** Internal append-only state beneath the shared owner's lifetime bind. This
 * is not an owner-election mechanism. Refuse damaged tails: discarding a slot
 * after a failed sync could allow a previously sent wake to be sent again. */
export function openCodexJournal(file: string, binding: object): {
  records: readonly unknown[]; append(value: unknown): void; close(): void;
} {
  file = path.resolve(file);
  if (writers.has(file)) throw new Error('Codex journal already has a writer');
  const directory = path.dirname(file), parent = lstatSync(directory);
  if (!parent.isDirectory() || parent.isSymbolicLink()) throw new Error('Codex journal requires a private directory');
  if (process.platform !== 'win32' && (parent.mode & 0o077)) throw new Error('Codex journal directory must be private');
  let fd: number | undefined;
  const nofollow = constants.O_NOFOLLOW ?? 0;
  try {
    try { fd = openSync(file, constants.O_RDWR | constants.O_APPEND | constants.O_CREAT | constants.O_EXCL | nofollow, 0o600); }
    catch (error) {
      if ((error as NodeJS.ErrnoException).code !== 'EEXIST') throw error;
      if (!lstatSync(file).isFile()) throw new Error('Codex journal must be a regular file');
      fd = openSync(file, constants.O_RDWR | constants.O_APPEND | nofollow);
    }
    const stat = fstatSync(fd);
    if (!stat.isFile() || stat.nlink !== 1 || (process.platform !== 'win32' && (stat.mode & 0o077))) {
      throw new Error('Codex journal must be a private regular file with one link');
    }
    const bytes = readFileSync(fd, 'utf8');
    if (bytes && !bytes.endsWith('\n')) throw new Error('Codex journal has an incomplete tail; recovery required');
    let previous = '', sequence = 0;
    const records: unknown[] = [];
    const identity = journalHash(binding);
    for (const line of bytes ? bytes.slice(0, -1).split('\n') : []) {
      const row = JSON.parse(line);
      const content = { version: row.version, binding: row.binding, sequence: row.sequence, previous: row.previous, value: row.value };
      if (row.version !== 1 || row.binding !== identity || row.sequence !== sequence + 1
          || row.previous !== previous || row.sha256 !== journalHash(content)) {
        throw new Error('Codex journal identity or chain differs; recovery required');
      }
      previous = row.sha256; sequence++; records.push(row.value);
    }
    // Also sync an existing entry: a previous create may have failed before
    // its directory sync. Success is never inferred merely from existence.
    fsyncSync(fd);
    if (process.platform !== 'win32') {
      const parentFd = openSync(directory, 'r');
      try { fsyncSync(parentFd); } finally { closeSync(parentFd); }
    }
    const descriptor = fd;
    let closed = false, failed = false;
    writers.add(file);
    return {
      records,
      append(value) {
        if (closed || failed) throw new Error('Codex journal is closed or requires recovery');
        try {
          if (!Number.isSafeInteger(sequence + 1)) throw new Error('Codex journal sequence exhausted');
          const content = { version: 1, binding: identity, sequence: sequence + 1, previous, value };
          const sha256 = journalHash(content);
          writeFileSync(descriptor, JSON.stringify({ ...content, sha256 }) + '\n');
          fsyncSync(descriptor);
          previous = sha256; sequence++;
        } catch (error) { failed = true; throw error; }
      },
      close() { if (!closed) { closed = true; writers.delete(file); closeSync(descriptor); } },
    };
  } catch (error) { if (fd !== undefined) closeSync(fd); throw error; }
}
