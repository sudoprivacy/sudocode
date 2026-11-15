/** ACP connection health and session input progress are different facts.
 * A diagnostic observation never resumes a paused session or retries input. */
export interface ScodeReceiveHealth {
  inputOwner: 'client' | 'session';
  state: 'unknown' | 'ready' | 'running' | 'paused' | 'error' | 'closed';
  revision?: number;
  pendingCount?: number;
  observedAt?: string;
  lastError?: { at: string; message: string };
}

export function observeScodeInput(current: ScodeReceiveHealth, sessionId: string,
  frame: unknown): ScodeReceiveHealth {
  const message = frame as {method?: unknown; params?: {sessionId?: unknown; update?: {
    kind?: unknown; message?: unknown; state?: {revision?: unknown; paused?: unknown;
      running?: unknown; pending?: unknown};
  }}} | null;
  if (!sessionId || message?.method !== '_sudocode/session/input_update'
      || message.params?.sessionId !== sessionId) return current;
  const update = message.params.update;
  if (update?.kind === 'error' && typeof update.message === 'string') {
    const at = new Date().toISOString();
    return {...current, state:'error', observedAt:at,
      lastError:{at,message:update.message.slice(0,2048)}};
  }
  const state = update?.state;
  if (update?.kind !== 'state' || !state || !Number.isSafeInteger(state.revision)
      || (state.revision as number) < 0 || typeof state.paused !== 'boolean'
      || typeof state.running !== 'boolean' || !Array.isArray(state.pending)
      || (current.revision !== undefined && (state.revision as number) <= current.revision)) return current;
  return {...current, state:state.paused?'paused':state.running?'running':'ready',
    revision:state.revision as number,pendingCount:state.pending.length,observedAt:new Date().toISOString()};
}
