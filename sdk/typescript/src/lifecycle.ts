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

let removeSignals: (() => void) | undefined;

/** The standalone CLI has one signal adapter for every ownership mode.
 * A rejected cleanup cannot be converted into process exit: doing so releases
 * OS ownership even when an old child might still be alive. Forced OS
 * termination remains outside this graceful guarantee.
 */
export function installCommandShutdownSignals(): () => void {
  if (removeSignals) throw new Error('Command signal handlers are already installed');
  let requested: Promise<void> | undefined;
  const handlers = ([['SIGINT', 130], ['SIGTERM', 143]] as const).map(([signal, code]) => {
    const handler = () => {
      requested ??= shutdownCommand();
      void requested.then(() => process.exit(code), error => {
        process.stderr.write(`Shutdown refused; cleanup UNKNOWN, ownership retained: ${String(error)}\n`);
        process.exitCode = 1;
      });
    };
    process.on(signal, handler);
    return { signal, handler };
  });
  let installed = true;
  removeSignals = () => {
    if (!installed) return;
    installed = false;
    for (const { signal, handler } of handlers) process.removeListener(signal, handler);
    removeSignals = undefined;
  };
  return removeSignals;
}
