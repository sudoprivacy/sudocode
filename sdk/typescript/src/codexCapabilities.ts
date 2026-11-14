/** Validate the running server's negotiated feature, never its version label
 * or the CLI executable used to reach a potentially older shared daemon. */
export function requireDurableCodexQueue(initialized: unknown): void {
  const response = initialized as {serverCapabilities?: {durableQueueAddOnce?: unknown}} | null;
  if (response?.serverCapabilities?.durableQueueAddOnce !== 1) {
    throw new Error('Native Codex did not negotiate durable thread/queue/addOnce version 1; A2A receiver admission refused before peer acceptance');
  }
}
