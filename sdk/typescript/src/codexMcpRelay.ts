import type { Socket } from 'node:net';
import type { Readable, Writable } from 'node:stream';
import { isDeepStrictEqual } from 'node:util';
import { STDIO_DEFAULT_MAX_BUFFER_SIZE } from '@modelcontextprotocol/sdk/shared/stdio.js';
import { timeout } from './timeout';

type Id = number | string;
interface Message {
  id?: Id;
  method?: string;
  params?: { requestId?: Id };
  result?: unknown;
}
interface Frame { bytes: Buffer; message: Message }

// Inspect routing fields while forwarding the original JSON and host metadata.
async function* frames(stream: Readable): AsyncGenerator<Frame> {
  let parts: Buffer[] = [], size = 0;
  for await (const data of stream) {
    const chunk: Buffer = Buffer.isBuffer(data) ? data : Buffer.from(data);
    let start = 0;
    while (start < chunk.length) {
      const newline = chunk.indexOf(10, start);
      const end = newline < 0 ? chunk.length : newline + 1;
      const part = chunk.subarray(start, end);
      size += part.length;
      if (size > STDIO_DEFAULT_MAX_BUFFER_SIZE) throw new Error('Oversized MCP stdio frame');
      parts.push(part);
      start = end;
      if (newline < 0) break;
      const bytes = parts.length === 1 ? part : Buffer.concat(parts, size);
      parts = []; size = 0;
      const message: Message = JSON.parse(bytes.toString('utf8'));
      if (!message || typeof message !== 'object' || Array.isArray(message)) throw new Error('Invalid MCP message');
      yield { bytes, message };
    }
  }
}

function write(stream: Writable, bytes: Buffer | string): Promise<void> {
  return new Promise((resolve, reject) => {
    stream.write(bytes, error => error ? reject(error) : resolve());
  });
}
function failure(id: Id, method: string, outcome: 'unknown' | 'not_submitted', detail: string): string {
  const data = { outcome, error: detail, automaticRetry: false };
  const reply = method === 'tools/call'
    ? { result: { isError: true, content: [{ type: 'text', text: JSON.stringify(data) }] } }
    : { error: { code: -32000, message: detail, data } };
  return JSON.stringify({ jsonrpc: '2.0', id, ...reply }) + '\n';
}

interface Peer {
  socket: Socket;
  sent: Map<Id, string>;
  ready: boolean;
  resume?: { id: Id; accept(result: unknown): void; reject(error: Error): void };
}

/** One frontend keeps its stdio lifetime. Only its negotiated MCP handshake
 * crosses a socket generation; every submitted application RPC belongs to
 * exactly one socket. */
export async function relayCodexMcp(first: Socket, connect: (signal: AbortSignal) => Promise<Socket>,
  input: Readable = process.stdin, output: Writable = process.stdout): Promise<void> {
  let peer: Peer | undefined, opening: Promise<void> | undefined;
  let init: Frame | undefined, negotiated: unknown, initialized: Frame | undefined;
  const lifetime = new AbortController();
  let stopped = false, queuedBytes = 0;
  const queued = new Map<Id, Frame>();
  // Server-initiated requests have their own ID space. An old frontend reply
  // must never satisfy a request from a different owner.
  const awaitingFrontend = new Map<Id, Peer | null>();
  const sendHost = (line: string | Buffer) => write(output, line);
  const dequeue = (id: Id) => {
    const frame = queued.get(id);
    if (frame) { queued.delete(id); queuedBytes -= frame.bytes.length; }
    return frame;
  };
  const forward = async (target: Peer, frame: Frame) => {
    const { id, method } = frame.message;
    if (method && id !== undefined) {
      target.sent.set(id, method);
      if (method === 'initialize') init = frame;
    }
    await write(target.socket, frame.bytes);
  };
  const refuseQueued = async (error: unknown) => {
    const rejected = [...queued];
    queued.clear(); queuedBytes = 0;
    for (const [id, frame] of rejected) {
      if (!stopped) await sendHost(failure(id, frame.message.method!, 'not_submitted', String(error)));
    }
  };
  const observe = async (target: Peer) => {
    try {
      for await (const frame of frames(target.socket)) {
        const { id, method, result } = frame.message;
        if (!method && id !== undefined) {
          if (target.resume && id === target.resume.id) {
            target.resume.accept(result);
            continue;
          }
          const original = target.sent.get(id);
          if (!original) continue;
          target.sent.delete(id);
          if (original === 'initialize' && result !== undefined) negotiated = result;
        } else if (method && id !== undefined) {
          if (awaitingFrontend.has(id)) {
            await write(target.socket, failure(id, method, 'not_submitted', 'Frontend request ID still outstanding'));
            continue;
          }
          awaitingFrontend.set(id, target);
        }
        await sendHost(frame.bytes);
      }
    } catch {
      // A reset, incomplete write or malformed response all leave sent calls ambiguous.
    } finally {
      target.socket.destroy();
      for (const [id, origin] of awaitingFrontend) {
        if (origin === target) awaitingFrontend.set(id, null);
      }
      const wasReady = target.ready;
      target.ready = false;
      if (peer === target) peer = undefined;
      target.resume?.reject(new Error('MCP owner disconnected during initialization'));
      // Launch the existing election once on loss; later requests can retry a
      // failed attachment. A refused handshake cannot create a tight retry loop.
      if (!stopped && wasReady && negotiated !== undefined) {
        void (opening ?? Promise.resolve()).then(() => {
          if (!stopped && !peer) return recover();
        }).catch(() => input.destroy(new Error('MCP frontend output closed')));
      }
      for (const [id, method] of target.sent) {
        if (!stopped) await sendHost(failure(id, method, 'unknown', 'MCP owner disconnected after submission'));
      }
    }
  };
  const attach = (socket: Socket): Peer => {
    const target: Peer = { socket, sent: new Map(), ready: false };
    peer = target;
    void observe(target).catch(() => input.destroy(new Error('MCP frontend output closed')));
    return target;
  };
  const recover = (): Promise<void> => {
    if (opening) return opening;
    opening = (async () => {
      const socket = await connect(lifetime.signal);
      if (stopped) { socket.destroy(); return; }
      const target = attach(socket);
      try {
        if (init && negotiated !== undefined) {
          const response = new Promise<unknown>((accept, reject) => {
            target.resume = { id: init!.message.id!, accept, reject };
          });
          const [, result] = await timeout(Promise.all([write(socket, init.bytes), response]), 3000, 'MCP reinitialization');
          target.resume = undefined;
          if (!isDeepStrictEqual(result, negotiated)) throw new Error('MCP owner initialization changed; frontend reinitialization required');
          if (initialized) await write(socket, initialized.bytes);
        }
        if (stopped) { socket.destroy(); return; }
        if (peer !== target || socket.destroyed) throw new Error('MCP owner disconnected during attachment');
        // Queue all admitted writes in FIFO order before accepting further
        // input. write callbacks confirm buffering, never execution.
        target.ready = true;
        const writes = [...queued.keys()].map(id => forward(target, dequeue(id)!));
        await Promise.all(writes);
      } catch (error) { socket.destroy(); throw error; }
    })().then(() => { opening = undefined; }, error => {
      opening = undefined;
      return refuseQueued(error);
    });
    return opening;
  };
  attach(first).ready = true;
  try {
    for await (const frame of frames(input)) {
      const { id, method, params } = frame.message;
      if (!method && id !== undefined) {
        const origin = awaitingFrontend.get(id);
        awaitingFrontend.delete(id);
        if (origin && origin === peer) await write(origin.socket, frame.bytes).catch(() => origin.socket.destroy());
        continue;
      }
      if (method === 'notifications/initialized') initialized = frame;
      if (method === 'notifications/cancelled' && params?.requestId !== undefined) {
        if (dequeue(params.requestId) || !peer?.sent.has(params.requestId)) continue;
      }
      if (method && id !== undefined && (queued.has(id) || peer?.sent.has(id))) {
        throw new Error('Duplicate outstanding MCP request ID');
      }
      if (peer?.ready && !peer.socket.destroyed) {
        const target = peer;
        await forward(target, frame).catch(() => target.socket.destroy());
      } else if (id !== undefined && method) {
        if (queuedBytes + frame.bytes.length > STDIO_DEFAULT_MAX_BUFFER_SIZE) {
          await sendHost(failure(id, method, 'not_submitted', 'MCP reconnect buffer cannot admit request'));
          continue;
        }
        queued.set(id, frame); queuedBytes += frame.bytes.length;
        void recover().catch(() => input.destroy(new Error('MCP frontend output closed')));
      }
    }
  } finally {
    stopped = true;
    lifetime.abort();
    queued.clear(); awaitingFrontend.clear();
    peer?.socket.destroy();
    await opening;
  }
}
