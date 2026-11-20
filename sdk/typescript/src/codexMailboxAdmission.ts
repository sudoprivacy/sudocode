import { z } from 'zod';
import { requestCodexMailboxPage } from './codexMailboxWake';
import type { CodexConnection } from './codexConnection';
import type { CodexMailboxAdmission } from './codexMailboxOwner';

/** Encoded bytes of one `scode_inbox` result and of one `thread/queue/add`
 * submission. Codex truncates a tool result to the tool's `output_token_limit`,
 * else to the model's policy, whose fallback is 10,000 bytes; the encoded
 * result bounds the text the model sees. The native mailbox fixture saw
 * presentations at 4096, 16384 and 32768 bytes arrive byte-for-byte. */
export const CODEX_MAILBOX_PRESENTATION_BYTES = 8192;

const page = z.object({ data: z.array(z.unknown()), nextCursor: z.string().nullish() });

/** Production peerInput v3 admission. Retirement never depends on it: scode
 * retires a frame only on that frame's exact ACK token, and the wake journal
 * bounds submissions per frame. Admission keeps every presentation inside the
 * conservative bound and accepts a thread once its actual server answers the
 * thread-scoped mailbox methods (queue, turns, history). Their item contents
 * are checked by every inventory read, not here. The answer is kept per server
 * connection and thread, so a reconnected or replaced server is checked again. */
export function codexMailboxAdmission(): CodexMailboxAdmission {
  const admitted = new WeakMap<object, Set<string>>();
  return {
    encodedResultBytes: CODEX_MAILBOX_PRESENTATION_BYTES,
    encodedSubmissionBytes: CODEX_MAILBOX_PRESENTATION_BYTES,
    async qualified(native: CodexConnection, threadId: string) {
      const server = native.initialized;
      if (!native.connected || !server || typeof server !== 'object') return false;
      if (admitted.get(server)?.has(threadId)) return true;
      try {
        page.parse(await native.request('thread/queue/list', { threadId, limit: 1 }));
        page.parse(await requestCodexMailboxPage(native, 'thread/turns/list', { threadId, limit: 1, sortDirection: 'desc', itemsView: 'full' }));
        z.object({ thread: z.object({ id: z.literal(threadId) }) })
          .parse(await native.request('thread/read', { threadId, includeTurns: false }));
      } catch {
        return false;
      }
      if (native.initialized !== server) return false;
      const threads = admitted.get(server) ?? new Set<string>();
      admitted.set(server, threads.add(threadId));
      return true;
    },
  };
}
