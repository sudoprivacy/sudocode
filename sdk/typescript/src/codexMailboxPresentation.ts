import { randomBytes, randomUUID } from 'node:crypto';
import type { CallToolResult } from '@modelcontextprotocol/sdk/types.js';
import { identifier } from './codexBindings';
import type { CodexConnection } from './codexConnection';
import type { ScodeInboxInput } from './scodeMailbox';

const capability = 'sudocode/mailbox-presentation';
const prefix = 'scode-presentation:';

/** A measurement through the registered inbox before an ACP receiver is
 * admitted. Only synthetic text is accessible. The embedding operator compares
 * it with the model-visible result independently of the raw MCP response. */
export class CodexMailboxPresentation {
  private readonly clients = new Map<string, { registrationId: string; boundThread?: string }>();
  private sample?: { native: CodexConnection; threadId: string; registrationId: string; connectionId: string; read: string; text: string };

  /** The elected owner calls this after authenticating the stdio connection. */
  attach(registrationId?: string, boundThread?: string) {
    if (registrationId !== undefined && !identifier(registrationId)) throw new Error('Invalid mailbox presentation registration');
    const connectionId = randomUUID();
    if (registrationId) this.clients.set(connectionId, { registrationId, boundThread });
    return {
      capabilities: registrationId ? { experimental: { [capability]: { version: 1, connectionId } } } : {},
      read: async (input: ScodeInboxInput, threadId: unknown): Promise<CallToolResult | undefined> => {
        if (!input.read?.startsWith(prefix)) return undefined;
        const sample = this.sample;
        if (!sample || sample.read !== input.read || threadId !== sample.threadId
            || sample.connectionId !== connectionId || Object.keys(input).length !== 1) {
          throw new Error('Mailbox presentation read differs from the prepared thread, connection or request');
        }
        const current = await this.currentConnection(sample.native, sample.threadId, sample.registrationId);
        if (this.sample !== sample || current !== connectionId) throw new Error('Mailbox presentation connection changed');
        this.sample = undefined;
        return { content: [{ type: 'text', text: sample.text }] };
      },
      close: () => {
        this.clients.delete(connectionId);
        if (this.sample?.connectionId === connectionId) this.sample = undefined;
      },
    };
  }

  cancel(read: string): void {
    if (this.sample?.read === read) this.sample = undefined;
  }

  private async currentConnection(native: CodexConnection, threadId: string, registrationId: string) {
    const page = await native.request<{ nextCursor: string | null; data: Array<{
      name: string; runtimeStatus: string; serverInfo?: { name?: string; description?: string } | null;
      serverCapabilities?: { experimental?: Record<string, { version?: number; connectionId?: string }> } | null;
      tools?: Record<string, unknown>; toolsError?: string | null;
    }> }>('mcpServerStatus/list', { threadId, serverName: 'nexus_scode', detail: 'full' });
    const server = page?.data?.[0];
    const observed = server?.serverCapabilities?.experimental?.[capability];
    const connectionId = observed?.connectionId;
    const client = connectionId && this.clients.get(connectionId);
    if (page?.nextCursor !== null || !Array.isArray(page.data) || page.data.length !== 1 || server?.name !== 'nexus_scode'
        || server.runtimeStatus !== 'connected' || server.serverInfo?.name !== 'nexus-scode'
        || server.serverInfo.description !== 'SCode registration ' + registrationId || server.toolsError != null
        || !Object.hasOwn(server.tools ?? {}, 'scode_inbox') || observed?.version !== 1 || !connectionId
        || !client || client.registrationId !== registrationId || (client.boundThread && client.boundThread !== threadId)) {
      throw new Error('Native thread has no matching live mailbox presentation connection');
    }
    return connectionId;
  }

  /** Selects the exact connection reported by the native thread. Does not
   * submit a queue item, start a turn, acknowledge a frame or admit a receiver. */
  async prepare(native: CodexConnection, threadId: string, registrationId: string, encodedResultBytes: number) {
    if (!identifier(threadId) || !identifier(registrationId) || !Number.isSafeInteger(encodedResultBytes)
        || encodedResultBytes < 4096 || encodedResultBytes > 65536) {
      throw new Error('Mailbox presentation requires an exact thread, registration and bounded result size');
    }
    const connectionId = await this.currentConnection(native, threadId, registrationId);
    if (!this.clients.has(connectionId)) throw new Error('Mailbox presentation connection closed');
    if (this.sample) throw new Error('A mailbox presentation read is already outstanding');
    const read = prefix + randomUUID();
    const envelope = JSON.stringify({ content: [{ type: 'text', text: '' }] });
    const length = encodedResultBytes - Buffer.byteLength(envelope);
    const text = randomBytes(Math.ceil(length * 3 / 4)).toString('base64url').slice(0, length);
    this.sample = { native, threadId, registrationId, connectionId, read, text };
    return { threadId, registrationId, connectionId, read, expectedText: text, encodedResultBytes };
  }
}
