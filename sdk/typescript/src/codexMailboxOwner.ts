import { existsSync } from 'node:fs';
import type { CodexConnection } from './codexConnection';
import type { MailboxScodeUpgrade } from './codexBindings';
import { openCodexMailboxMigration } from './codexMailboxMigration';
import { mailboxSubmissionTextBudget, openCodexMailboxWake, readCodexMailboxSnapshot, readCodexMailboxThread } from './codexMailboxWake';
import { mailboxTextBudget, openScodeMailbox, scodeMailboxHeads, type ScodeMailbox } from './scodeMailbox';
import type { ScodeConnection } from './scodeAcp';

/** Admission of one v3 receiver: both presentation bounds and a check of the
 * actual server connection, run before every mailbox call. The production
 * provider is `codexMailboxAdmission`. */
export interface CodexMailboxAdmission {
  encodedResultBytes: number;
  encodedSubmissionBytes: number;
  /** Adopt one already bound session. Other loaded roots and their enrollment
   * operations stay outside this owner's scope. */
  existingBinding?: Readonly<{ threadId: string; actor: string; acpSessionId: string; scodeUpgrade?: MailboxScodeUpgrade }>;
  qualified(native: CodexConnection, threadId: string): Promise<boolean>;
}

/** Compose one already verified v3 receiver under the shared owner's lifetime
 * fence. The old v2 owner must have exited and joined before this function. */
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
  let migration: Awaited<ReturnType<typeof openCodexMailboxMigration>> | undefined;
  let raw: ScodeMailbox | undefined;
  let wake: ReturnType<typeof openCodexMailboxWake> | undefined;
  try {
    const inbox = callbackLog + '.inbox';
    if (existsSync(inbox)) {
      migration = await openCodexMailboxMigration(inbox, callbackLog + '.migration.jsonl',
        { threadId, sessionId: connection.sessionId }, async pendingIds => {
          if (pendingIds.length) {
            const view = await readCodexMailboxSnapshot(native, threadId, () => new Set(pendingIds));
            if (!view.loaded) throw new Error('Legacy cutover requires a loaded thread and complete native inventory');
            await requireQualified();
            return new Set([...view.queued, ...view.running, ...view.finished]);
          }
          const thread = await readCodexMailboxThread(native, threadId, false);
          if (thread.status.type === 'notLoaded') throw new Error('Legacy cutover requires a loaded thread');
          await requireQualified();
          return new Set<string>();
        });
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
        const heads = scodeMailboxHeads(offer);
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
