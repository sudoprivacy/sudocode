import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { randomBytes, randomUUID } from 'node:crypto';
import { statSync } from 'node:fs';
import { createServer, type Socket } from 'node:net';
import * as path from 'node:path';
import { systemExecutable } from './windowsCodexOwner';
import { windowsProxyJobSource } from './windowsProxyJobSource';

export class WindowsProxyCleanupError extends Error {}

export interface OwnedProxy {
  child: ChildProcessWithoutNullStreams;
  pid: number | undefined;
  exited: Promise<{ code: number | null; signal: NodeJS.Signals | null }>;
  stop(force: boolean): void;
}

function quoteArgument(value: string): string {
  return '"' + value.replace(/(\\*)"/g, (_, slashes: string) => slashes + slashes + '\\"')
    .replace(/\\+$/, slashes => slashes + slashes) + '"';
}

function nativeLaunch(argv: string[], env: NodeJS.ProcessEnv): string {
  // As in Node's Windows spawn, use the first spelling of each environment
  // key in lexical order. Keep the native environment separate from the keeper.
  const variables = new Map<string, string>();
  let searchPath = process.env.PATH;
  for (const key of Object.keys(env).sort()) {
    const name = key.toUpperCase();
    const value = env[key];
    if (value === undefined || variables.has(name)) continue;
    variables.set(name, key + '=' + value);
    if (name === 'PATH') searchPath = value;
  }
  const ordered = [...variables.keys()].sort().map(key => variables.get(key)!);
  const environment = Buffer.from(ordered.join('\0') + '\0\0', 'utf16le').toString('base64');
  // CreateProcess otherwise searches the keeper's PATH. Bind the executable
  // to the caller's PATH, with the same .com/.exe order as Node/libuv.
  const file = argv[0];
  const hasDir = /[\\/:]/.test(file);
  const dirs = hasDir ? [''] : [
    ...(process.env.NoDefaultCurrentDirectoryInExePath === undefined ? [''] : []),
    ...(searchPath ?? '').split(';').filter(Boolean).map(dir => dir.replace(/^["']|["']$/g, '')),
  ];
  const names = [...(hasDir || /\..+/.test(path.basename(file)) ? [file] : []), file + '.com', file + '.exe'];
  for (const dir of dirs) {
    for (const name of names) {
      const executable = path.resolve(dir, name);
      if (statSync(executable, { throwIfNoEntry: false })?.isFile()) {
        return JSON.stringify({ executable, command: argv.map(quoteArgument).join(' '), environment }) + '\n';
      }
    }
  }
  throw Object.assign(new Error('spawn ' + file + ' ENOENT'), { code: 'ENOENT' });
}

/** The keeper inherits the three transport streams. A separate authenticated
 * pipe requests shutdown and confirms that the native process's Job is empty. */
export async function spawnWindowsProxy(argv: string[], env: NodeJS.ProcessEnv): Promise<OwnedProxy> {
  const request = nativeLaunch(argv, env);
  const executable = systemExecutable(path.join('WindowsPowerShell', 'v1.0', 'powershell.exe'));
  const name = 'sudocode-proxy-' + randomUUID();
  const token = randomBytes(32).toString('hex');
  const server = createServer();
  await new Promise<void>((resolve, reject) => {
    server.once('error', reject);
    server.listen('\\\\.\\pipe\\' + name, resolve);
  });
  const source = windowsProxyJobSource + "\nStart-ProxyJob '" + name + "' '" + token + "'";
  let child: ChildProcessWithoutNullStreams;
  try {
    child = spawn(executable, ['-NoLogo', '-NoProfile', '-NonInteractive', '-EncodedCommand',
      Buffer.from(source, 'utf16le').toString('base64')], { stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true });
  } catch (error) {
    server.close();
    throw error;
  }
  let control: Socket | undefined, submitted = false, drained = false, stopping = false;
  let problem: Error | undefined;
  let ready!: (pid: number) => void, failed!: (error: Error) => void;
  const started = new Promise<number>((resolve, reject) => { ready = resolve; failed = reject; });
  const clients = new Set<Socket>();
  server.on('connection', socket => {
    clients.add(socket);
    socket.on('error', () => {});
    socket.once('close', () => clients.delete(socket));
    socket.setEncoding('utf8');
    let buffer = '';
    socket.on('data', chunk => {
      buffer += chunk;
      if (buffer.length > 8192) { socket.destroy(); return; }
      let end: number;
      while ((end = buffer.indexOf('\n')) !== -1) {
        const line = buffer.slice(0, end).replace(/\r$/, '');
        buffer = buffer.slice(end + 1);
        if (!control && line === token && !stopping) {
          control = socket;
          server.close();
          for (const other of clients) if (other !== socket) other.destroy();
          submitted = true;
          socket.write(request);
        } else if (socket !== control) {
          socket.destroy();
          return;
        } else if (line === 'drained') {
          drained = true;
        } else if (line.startsWith('error ')) {
          problem = new Error('Windows Codex proxy exited: ' + line.slice(6));
        } else if (/^[1-9][0-9]*$/.test(line)) {
          ready(Number(line));
        }
      }
    });
  });
  let diagnostics = '';
  const diagnostic = (bytes: Buffer) => { diagnostics = (diagnostics + bytes.toString()).slice(-2048); };
  child.stderr.on('data', diagnostic);
  const exited: OwnedProxy['exited'] = new Promise((resolve, reject) => {
    child.once('error', error => { problem = error; });
    child.once('close', async (code, signal) => {
      // The last control receipt can arrive after ChildProcess.close.
      if (control && !control.readableEnded && !control.destroyed) {
        await new Promise<void>(done => control!.once('close', done));
      }
      failed(problem ?? new Error(`Windows Codex proxy keeper exited ${code}: ${diagnostics}`));
      if (submitted && !drained) reject(new WindowsProxyCleanupError('Windows Codex proxy Job exited without a drain receipt'));
      else resolve({ code, signal });
    });
  });
  void exited.catch(() => {});
  const stop = () => {
    if (stopping) return;
    stopping = true;
    if (submitted && control) control.end('stop\n');
    else child.kill();
  };
  const timer = setTimeout(() => failed(new Error('Windows Codex proxy bootstrap timed out')), 15000);
  try {
    const pid = await started;
    return { child, pid, exited, stop };
  } catch (error) {
    stop();
    await exited;
    throw error;
  } finally {
    clearTimeout(timer);
    child.stderr.removeListener('data', diagnostic);
    if (server.listening) server.close();
    for (const socket of clients) if (socket !== control) socket.destroy();
  }
}
