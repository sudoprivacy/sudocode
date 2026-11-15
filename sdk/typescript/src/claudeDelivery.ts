import * as fs from 'node:fs';
import * as path from 'node:path';
import { performance } from 'node:perf_hooks';
import { z } from 'zod';
import type { ScodeConnection } from './scodeAcp';
import { mailboxTextBudget, openScodeMailbox, scodeMailboxHeads, type ScodeMailbox, type ScodeMailboxHead } from './scodeMailbox';
import { journalHash, openCodexJournal } from './codexJournal';
import { CODEX_MAILBOX_DELIVERY_PROVENANCE } from './codexMailboxWake';
import { appendClaudeDelivery, createExclusive } from './claudeWake';

/** Encoded bound of a `scode_inbox` result. */
const RESULT_BYTES = 65536;
/** Claude Code caps the hook output it adds to the context at 10,000
 * characters. A UTF-8 byte is at most one character, so a delivery fits. */
const HOOK_OUTPUT_CHARACTERS = 10_000;
const REDELIVERY_MS = 10 * 60_000;
const EVALUATE_MS = 60_000;
const HEARTBEAT_MS = 5_000;
const STALE_MS = 30_000;
const STANDBY = 'another bridge owns this session\'s deliveries';

const nonempty = z.string().min(1);
const keySchema = z.object({ conversation: nonempty, start: z.number().int().nonnegative().safe(), epoch: nonempty });
const slotSchema = z.object({ key: keySchema, n: z.number().int().min(1).max(3) });
const GENERATION = /^[1-9][0-9]*$/;

export interface ClaudeDeliveryStatus {
  state: 'standby' | 'detached' | 'idle' | 'waiting' | 'submitted' | 'delivery_exhausted' | 'blocked' | 'closed';
  /** `<hash of the budget key>:<n>` of the latest delivery. */
  delivery?: string;
  deliveryId?: string;
  submissions?: number;
  /** The live bridge that owns deliveries while this one stands by. */
  owner?: number;
  detail?: string;
  error?: string;
}

export interface ClaudeDelivery {
  /** The bound Agent; the loaded scode session must prove it is this one. */
  readonly actor: string;
  /** The native root the mailbox binds: the Claude Code session id. */
  readonly threadId: string;
  /** The session called a bridge tool, which renews the delivery epoch. */
  called(): void;
  /** Open the mailbox of a connection whose Node identity was verified. */
  attach(connection: ScodeConnection): ScodeMailbox;
  /** Stop delivering from the attached mailbox and drain its acknowledgements. */
  detach(): Promise<void>;
  /** scode's unread count changed. */
  unread(count: number): void;
  status(): ClaudeDeliveryStatus;
  close(): Promise<void>;
}

/**
 * Budgeted delivery of one Claude Code session's mailbox offers into its spool.
 *
 * A budget key is (conversation, start of its head frame, epoch). The epoch is
 * the session's latest call to any bridge tool: the mtime of
 * `<session>.delivery.epoch`, which every bridge of the session touches, or
 * `none`. Each key has three deliveries, each logged and synced before its
 * spool entry is written. A head is delivered again only while it is unread
 * and at least ten minutes after this bridge last presented it. Only the
 * model's acknowledgement through `scode_inbox` retires a frame.
 *
 * One bridge per session writes the log and the spool. Its claim is the
 * highest generation in `<session>.delivery.owner/`, created exclusively (a
 * hard link of a complete file) and holding its pid; its mtime is a heartbeat.
 * Another bridge stands by while the claim is fresh and its pid lives, and
 * takes over a stale or dead claim by creating the next generation.
 */
export function openClaudeDelivery(options: { stateDirectory: string; sessionId: string; actor: string; spool: string }): ClaudeDelivery {
  const { sessionId, actor, spool } = options;
  const base = path.join(options.stateDirectory, `${sessionId}.delivery`);
  const owners = base + '.owner', epochFile = base + '.epoch';
  fs.mkdirSync(owners, { recursive: true, mode: 0o700 });
  const claimFile = (generation: number) => path.join(owners, String(generation));
  const top = () => Math.max(0, ...fs.readdirSync(owners).filter(name => GENERATION.test(name)).map(Number));
  const alive = (pid: unknown) => {
    if (typeof pid !== 'number' || !Number.isSafeInteger(pid) || pid <= 0) return false;
    try { process.kill(pid, 0); return true; } catch (error) { return (error as NodeJS.ErrnoException).code === 'EPERM'; }
  };
  const headId = (head: ScodeMailboxHead) => JSON.stringify([head.conversation, head.start]);
  const maxTextBytes = Math.min(mailboxTextBudget(RESULT_BYTES),
    HOOK_OUTPUT_CHARACTERS - CODEX_MAILBOX_DELIVERY_PROVENANCE.length - 1);

  let generation: number | undefined, holder: number | undefined, failure: string | undefined;
  let log: ReturnType<typeof openCodexJournal> | undefined;
  let mailbox: ScodeMailbox | undefined;
  const slots = new Map<string, number>(), presented = new Map<string, number>();
  let status: ClaudeDeliveryStatus = { state: 'idle' };
  let closed = false, chain: Promise<void> = Promise.resolve();
  const owns = () => generation !== undefined && top() === generation;

  /** Become the writer unless a live bridge holds a fresh claim. */
  const claim = (): boolean => {
    const current = top();
    if (current) {
      try {
        const file = claimFile(current), { pid } = JSON.parse(fs.readFileSync(file, 'utf8')) as { pid?: unknown };
        if (Date.now() - fs.statSync(file).mtimeMs < STALE_MS && alive(pid)) { holder = pid as number; return false; }
      } catch (error) {
        // A newer generation replaced it meanwhile; look again on the next beat.
        if ((error as NodeJS.ErrnoException).code === 'ENOENT') return false;
        throw error;
      }
    }
    if (!createExclusive(owners, String(current + 1), JSON.stringify({ pid: process.pid }))) return false;
    generation = current + 1; holder = undefined;
    for (const name of fs.readdirSync(owners)) {
      if (GENERATION.test(name) && Number(name) < generation) fs.rmSync(path.join(owners, name), { force: true });
    }
    return true;
  };
  const own = () => {
    const journal = openCodexJournal(base + '.jsonl', { kind: 'claude-mailbox-delivery-v1', sessionId, actor });
    try {
      slots.clear();
      for (const value of journal.records) {
        const row = slotSchema.parse(value), hash = journalHash(row.key);
        if (row.n !== (slots.get(hash) ?? 0) + 1) throw new Error('Delivery journal slots are not contiguous');
        slots.set(hash, row.n);
      }
    } catch (error) { journal.close(); throw error; }
    log = journal; status = { state: 'idle' };
    void evaluate();
  };
  const beat = () => {
    if (closed) return;
    try {
      if (generation === undefined) { if (claim()) own(); }
      else if (!owns()) { log?.close(); log = undefined; generation = undefined; }
      else {
        const now = new Date();
        fs.utimesSync(claimFile(generation), now, now);
        if (!log) own();
      }
      failure = undefined;
    } catch (error) { failure = String(error); }
  };

  const epoch = () => {
    try { return String(Math.trunc(fs.statSync(epochFile).mtimeMs)); } catch (error) {
      if ((error as NodeJS.ErrnoException).code === 'ENOENT') return 'none';
      throw error;
    }
  };
  const evaluate = () => {
    const task = chain.then(async () => {
      if (closed || !log || !mailbox) return;
      try {
        const offer = await mailbox.inspect();
        const journal = log;
        if (closed || !journal || !owns()) return;
        const heads = scodeMailboxHeads(offer);
        if (offer.unread && !heads.length) throw new Error('Unread mailbox has no offered head');
        if (!heads.length) { presented.clear(); status = { state: 'idle' }; return; }
        if (!offer.text) throw new Error('Unread mailbox offer has no presentation');
        const now = performance.now(), current = epoch();
        let exhausted: ClaudeDeliveryStatus | undefined;
        for (const head of heads) {
          const key = { conversation: head.conversation, start: head.start, epoch: current };
          const hash = journalHash(key), n = (slots.get(hash) ?? 0) + 1;
          if (n > 3) { exhausted ??= { state: 'delivery_exhausted', deliveryId: head.deliveryId, submissions: 3 }; continue; }
          const shown = presented.get(headId(head));
          if (shown !== undefined && now - shown < REDELIVERY_MS) continue;
          // The number is spent before the entry exists, even if the write fails.
          journal.append({ key, n }); slots.set(hash, n);
          for (const each of heads) presented.set(headId(each), now);
          const delivery = `${hash}:${n}`;
          status = { state: 'submitted', delivery, deliveryId: head.deliveryId, submissions: n };
          appendClaudeDelivery(spool, { delivery, heads, text: CODEX_MAILBOX_DELIVERY_PROVENANCE + '\n' + offer.text });
          return;
        }
        status = exhausted ?? { state: 'waiting' };
      } catch (error) { status = { ...status, state: 'blocked', error: String(error) }; }
    });
    chain = task; return task;
  };

  beat();
  const heartbeat = setInterval(beat, HEARTBEAT_MS);
  const periodic = setInterval(() => { void evaluate(); }, EVALUATE_MS);
  heartbeat.unref(); periodic.unref();
  const release = async () => {
    const previous = mailbox;
    mailbox = undefined;
    await chain;
    await previous?.close();
  };
  return {
    actor,
    threadId: sessionId,
    called() {
      try {
        const now = new Date();
        try { fs.utimesSync(epochFile, now, now); } catch (error) {
          if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error;
          fs.writeFileSync(epochFile, '', { flag: 'a', mode: 0o600 });
        }
      } catch (error) { process.stderr.write(`Delivery epoch not renewed: ${String(error)}\n`); }
    },
    attach(connection) {
      const opened = openScodeMailbox(connection, sessionId, RESULT_BYTES, undefined, maxTextBytes);
      mailbox = opened;
      void evaluate();
      return opened;
    },
    detach: release,
    unread(count) {
      if (count > 0) { void evaluate(); return; }
      // Nothing is unread, so there is no head to deliver or to space out.
      chain = chain.then(() => { presented.clear(); status = { state: 'idle' }; });
    },
    status() {
      if (closed) return { state: 'closed' };
      if (failure) return { state: 'blocked', error: failure };
      if (generation === undefined) return { state: 'standby', detail: STANDBY, ...(holder ? { owner: holder } : {}) };
      if (!mailbox) return { state: 'detached' };
      return { ...status };
    },
    async close() {
      if (closed) return;
      closed = true;
      clearInterval(heartbeat); clearInterval(periodic);
      await release();
      log?.close(); log = undefined;
      // A stale heartbeat lets a standing-by bridge take over on its next beat.
      try { if (owns()) fs.utimesSync(claimFile(generation!), 0, 0); } catch { /* it takes over after STALE_MS */ }
      generation = undefined;
    },
  };
}
