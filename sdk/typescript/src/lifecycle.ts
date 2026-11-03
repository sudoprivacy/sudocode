let shutdown: (() => Promise<void>) | undefined;

/** Foreground connection commands keep ownership until explicit EOF or shutdown. */
export function registerCommandShutdown(handler: () => Promise<void>): () => void {
  if (shutdown) {
    throw new Error('A foreground command already owns the CLI lifetime');
  }
  shutdown = handler;
  return () => { if (shutdown === handler) shutdown = undefined; };
}

export async function shutdownCommand(): Promise<void> {
  await shutdown?.();
}
