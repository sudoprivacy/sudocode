import { existsSync } from 'node:fs';
import type { CodexConnection } from './codexConnection';
import { openCodexMailboxMigration } from './codexMailboxMigration';
import { mailboxSubmissionTextBudget, openCodexMailboxWake, readCodexMailboxSnapshot, type CodexMailboxHead } from './codexMailboxWake';
import { mailboxTextBudget, openScodeMailbox, type ScodeMailbox, type ScodeMailboxOffer } from './scodeMailbox';
import type { ScodeConnection } from './scodeAcp';

/** Operator-provided admission, not a serializable "qualified" switch. The
 * provider must bind its evidence to the actual server and effective thread
 * settings, including both presentation bounds, and recheck them on every call. */
export interface CodexMailboxAdmission {
  encodedResultBytes: number;
  encodedSubmissionBytes: number;
  qualified(native: CodexConnection, threadId: string): Promise<boolean>;
}

function frameHeads(offer: ScodeMailboxOffer): CodexMailboxHead[] {
  const heads = new Map<string, CodexMailboxHead>();
  for (const frame of offer.frames) {
    if (!frame.frameId.startsWith(frame.conversation + ':')) throw new Error('Mailbox frame conversation differs');
    const offsets = /^:(0|[1-9][0-9]*)-(0|[1-9][0-9]*)$/.exec(frame.frameId.slice(frame.conversation.length));
    const start = Number(offsets?.[1]), end = Number(offsets?.[2]);
    if (!offsets || !Number.isSafeInteger(start) || !Number.isSafeInteger(end) || end <= start
        || !frame.deliveryIds.length) throw new Error('Mailbox frame lacks its original range or delivery identity');
    const previous = heads.get(frame.conversation);
    if (!previous || start < previous.start) heads.set(frame.conversation,
      { conversation: frame.conversation, start, deliveryId: frame.deliveryIds[0] });
  }
  return [...heads.values()];
}

/** Compose one already verified v3 receiver under the shared owner's lifetime
 * fence. The old v2 owner must have exited and joined before this function.
 * No production file-config adapter can enable this without native admission. */
export async function openCodexMailboxOwner(options: {
  actor: string; threadId: string; connection: ScodeConnection; native: CodexConnection;
  callbackLog: string; admission: CodexMailboxAdmission;
}) {
  const { actor, threadId, connection, native, callbackLog, admission } = options;
  let closed = false;
  const qualified = () => closed ? Promise.resolve(false) : admission.qualified(native, threadId);
  const requireQualified = async () => {
    if (!await qualified()) throw new Error('native_unqualified: actual owning server and queue/tool presentation settings require qualification');
  };
  await requireQualified();
  let migration: ReturnType<typeof openCodexMailboxMigration> | undefined;
  let raw: ScodeMailbox | undefined;
  let wake: ReturnType<typeof openCodexMailboxWake> | undefined;
  try {
    const inbox = callbackLog + '.inbox';
    if (existsSync(inbox)) {
      const view = await readCodexMailboxSnapshot(native, threadId);
      if (!view.loaded) throw new Error('Legacy cutover requires a loaded thread and complete native inventory');
      await requireQualified();
      migration = openCodexMailboxMigration(inbox, callbackLog + '.migration.jsonl',
        { threadId, sessionId: connection.sessionId }, new Set([...view.queued, ...view.running, ...view.finished]));
    }
    const maxTextBytes = Math.min(mailboxTextBudget(admission.encodedResultBytes),
      mailboxSubmissionTextBudget(actor, threadId, admission.encodedSubmissionBytes));
    raw = openScodeMailbox(connection, threadId, admission.encodedResultBytes, migration, maxTextBytes);
    const mailbox: ScodeMailbox = {
      async inbox(input) {
        await requireQualified();
        const result = await raw!.inbox(input);
        void wake?.evaluate();
        return result;
      },
      async inspect() { await requireQualified(); return raw!.inspect(); },
      async close() { await close(); },
    };
    wake = openCodexMailboxWake({ actor, threadId, native, qualified,
      encodedSubmissionBytes: admission.encodedSubmissionBytes,
      journal: callbackLog + '.wake.jsonl',
      async offer() {
        const offer = await mailbox.inspect();
        // Legacy receipts have no mailbox offset. Their immutable event IDs
        // form separate keys until the scode commit closes their ledger rows.
        const legacy = offer.legacyHead;
        const heads = frameHeads(offer);
        if (legacy) heads.unshift({ conversation: `legacy:${legacy.eventId}`, start: 0, deliveryId: legacy.deliveryId });
        if (offer.unread && !heads.length) throw new Error('Unread mailbox has no offered head');
        return { heads, text: offer.text };
      },
    });
    let closing: Promise<void> | undefined;
    const close = () => closing ??= (async () => {
      closed = true;
      // Prevent new tool calls, drain the wake reader and all ACKs, then close
      // the migration writer. The outer owner separately joins its ACP child.
      const results = await Promise.allSettled([wake!.close(), raw!.close()]);
      migration?.close();
      const failed = results.find(result => result.status === 'rejected');
      if (failed?.status === 'rejected') throw failed.reason;
    })();
    return { mailbox, evaluate: wake.evaluate, notify: wake.notify, status: wake.status, close };
  } catch (error) {
    await wake?.close(); await raw?.close(); migration?.close(); throw error;
  }
}
