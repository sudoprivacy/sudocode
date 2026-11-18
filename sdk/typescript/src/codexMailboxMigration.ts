import { createHash } from 'node:crypto';
import { lstatSync, readFileSync, readdirSync } from 'node:fs';
import * as path from 'node:path';
import { z } from 'zod';
import { journalHash, openCodexJournal } from './codexJournal';
import type { ScodePeerMessage } from './scodeAcp';
import type { ScodeLegacyOffer, ScodeMailboxOffer } from './scodeMailbox';

const count = z.number().int().nonnegative().safe();
const receipt = z.object({
  version: z.literal(1), threadId: z.string(), sessionId: z.string(), eventId: z.string(),
  messageHash: z.string().regex(/^[a-f0-9]{64}$/), message: z.unknown().optional(),
  receivedAt: z.number().finite(), sequence: count.refine(n => n > 0),
  state: z.enum(['pending', 'done']), attempts: count,
});
const outcome = z.object({ eventId: z.string(), receiptSha256: z.string(),
  state: z.enum(['native_custody', 'offered', 'committed']), possiblyDelivered: z.boolean() });

/** Call only after joining the old v2 owner. Pending receipts require every
 * native queue and history page. Done receipts never transfer custody.
 * The separate journal contains outcomes and hashes, never message bodies. */
export async function openCodexMailboxMigration(inbox: string, ledger: string,
  binding: { threadId: string; sessionId: string },
  checkNative: (pendingEventIds: readonly string[]) => Promise<ReadonlySet<string>>): Promise<{
    pending(): readonly ScodeLegacyOffer[];
    committed(commits: ScodeMailboxOffer['commits']): void;
    close(): void;
  }> {
  if (!lstatSync(inbox).isDirectory()) throw new Error('Legacy inbox must be a regular directory');
  const snapshot = () => {
    const rows = readdirSync(inbox).filter(name => /^[a-f0-9-]{36}\.json$/.test(name)).sort().map(name => {
      const file = path.join(inbox, name);
      if (!lstatSync(file).isFile()) throw new Error('Legacy receipt must be a regular file');
      const bytes = readFileSync(file);
      return { name, sha256: createHash('sha256').update(bytes).digest('hex'), record: receipt.parse(JSON.parse(bytes.toString('utf8'))) };
    });
    const sequences = new Set<number>();
    for (const { name, record } of rows) {
      if (record.threadId !== binding.threadId || record.sessionId !== binding.sessionId
          || name !== record.eventId + '.json' || sequences.has(record.sequence)
          || (record.state === 'pending' && (!record.message || journalHash(record.message) !== record.messageHash))) {
        throw new Error('Legacy inbox identity, order or content differs; recovery required');
      }
      sequences.add(record.sequence);
      if (record.state === 'pending') {
        const message = record.message as ScodePeerMessage;
        if (message.sessionId !== binding.sessionId || typeof message.text !== 'string'
            || typeof message.body !== 'string' || typeof message.sudocode?.a2a?.from !== 'string'
            || !message.sudocode.a2a.from) throw new Error('Invalid legacy peer message');
      }
    }
    return rows;
  };
  const original = snapshot();
  const fingerprint = journalHash(original.map(row => [row.name, row.sha256]));
  const pendingRows = original.filter(row => row.record.state === 'pending');
  const nativeClientIds = await checkNative(pendingRows.map(row => row.record.eventId));
  const checkFingerprint = () => {
    if (journalHash(snapshot().map(row => [row.name, row.sha256])) !== fingerprint) {
      throw new Error('Legacy inbox changed after cutover; recovery required');
    }
  };
  checkFingerprint();
  const journal = openCodexJournal(ledger, { kind: 'codex-v2-migration', ...binding, inbox: path.resolve(inbox), fingerprint });
  const states = new Map<string, z.infer<typeof outcome>>();
  try {
    for (const value of journal.records) {
      const row = outcome.parse(value), source = original.find(item => item.record.eventId === row.eventId);
      const previous = states.get(row.eventId);
      if (!source || source.record.state !== 'pending' || source.sha256 !== row.receiptSha256
          || row.possiblyDelivered !== (source.record.attempts > 0)
          || (previous ? previous.state !== 'offered' || row.state !== 'committed'
              || row.possiblyDelivered !== previous.possiblyDelivered : row.state === 'committed')) {
        throw new Error('Legacy migration ledger contradicts its original receipts');
      }
      states.set(row.eventId, row);
    }
    for (const row of pendingRows) {
      if (states.has(row.record.eventId)) continue;
      const value = { eventId: row.record.eventId, receiptSha256: row.sha256,
        state: nativeClientIds.has(row.record.eventId) ? 'native_custody' as const : 'offered' as const,
        possiblyDelivered: row.record.attempts > 0 };
      journal.append(value); states.set(value.eventId, value);
    }
    let closed = false;
    const unchanged = () => {
      if (closed) throw new Error('Legacy migration is closed');
      checkFingerprint();
    };
    return {
      pending() {
        unchanged();
        return original.filter(row => states.get(row.record.eventId)?.state === 'offered')
          .sort((a, b) => a.record.sequence - b.record.sequence)
          .map(({ record }) => ({ eventId: record.eventId, possiblyDelivered: record.attempts > 0,
            message: JSON.parse(JSON.stringify(record.message)) as ScodePeerMessage }));
      },
      committed(commits) {
        unchanged();
        for (const commit of commits) {
          if (!('legacyEventId' in commit) || commit.status !== 'committed') continue;
          const previous = states.get(commit.legacyEventId);
          if (!previous) throw new Error('scode committed an unknown legacy entry');
          if (previous.state === 'committed') continue;
          if (previous.state !== 'offered') throw new Error('scode committed an entry already in native custody');
          const next = { ...previous, state: 'committed' as const };
          journal.append(next); states.set(next.eventId, next);
        }
      },
      close() { closed = true; journal.close(); },
    };
  } catch (error) { journal.close(); throw error; }
}
