import { execFile } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { accessSync, appendFileSync, closeSync, constants, existsSync, fsyncSync, mkdirSync, openSync, readFileSync, readdirSync,
  renameSync, statSync, unlinkSync, writeFileSync } from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { promisify } from 'node:util';
import { prepareCodexEnrollment } from './codexEnrollment';

const execute = promisify(execFile);

/** One exact command the caller can run next, and why. */
export interface SetupStep { why: string; argv: string[] }

export type SetupState = 'installed' | 'already_installed' | 'moved_to_this_sdk' | 'needs_input' | 'refused' | 'failed';

/** What a setup command prints: one object a person or a model can act on
 * without reading documentation. `instructions` are plain sentences in the
 * order to follow them; `next` holds exact commands. */
export interface SetupResult {
  ok: boolean;
  frontend: 'claude' | 'codex';
  state: SetupState;
  changed: string[];
  verified: string[];
  notVerified: string[];
  instructions: string[];
  next: SetupStep[];
  changedAnything?: boolean;
  journal?: string;
  error?: string;
}

export interface SetupOptions {
  /** This SDK's `dist/server.cjs`: the bridge the frontend will start. */
  entry: string;
  /** The command the caller typed, repeated in `next` (default: node + entry + flag). */
  callerArgv?: string[];
  issuer?: string;
  nodeDataDir?: string;
  nexusEnroll?: string;
  /** Test seam and advanced use: the enrollment tool as a full argv. */
  nexusEnrollArgv?: string[];
  scode?: string;
  configRoot?: string;
  environment?: NodeJS.ProcessEnv;
}

/** Setup stopped before or without completing, with what the caller should do. */
export class SetupStop extends Error {
  constructor(readonly state: 'needs_input' | 'refused', message: string,
    readonly instructions: string[], readonly next: SetupStep[] = []) {
    super(message);
  }
}

export interface SharedSetup {
  root: string;
  journal: string;
  enrollmentArgv: string[];
  changed: string[];
  verified: string[];
}

export function journalLine(journal: string, record: Record<string, unknown>): void {
  mkdirSync(path.dirname(journal), { recursive: true, mode: 0o700 });
  appendFileSync(journal, JSON.stringify({ at: new Date().toISOString(), ...record }) + '\n', { mode: 0o600 });
}

/** Replace `file` in one step, so a reader never sees half of it. */
export function atomicWrite(file: string, bytes: Buffer | string, mode: number): void {
  const temporary = path.join(path.dirname(file), `.${path.basename(file)}.${randomUUID()}`);
  const handle = openSync(temporary, 'wx', mode);
  try { writeFileSync(handle, bytes); fsyncSync(handle); } finally { closeSync(handle); }
  try { renameSync(temporary, file); } catch (error) { try { unlinkSync(temporary); } catch { /* Best effort. */ } throw error; }
}

/** Quote one word for a POSIX shell; plain paths stay readable. */
export function shellWord(value: string): string {
  return /^[A-Za-z0-9_@%+=:,./-]+$/.test(value) ? value : "'" + value.replace(/'/g, "'\\''") + "'";
}

/** Split a command line the way a POSIX shell would for quotes and blanks. */
export function splitShellWords(command: string): string[] {
  const words: string[] = [];
  let word = '';
  let quote: '"' | "'" | undefined;
  let open = false;
  for (let i = 0; i < command.length; i++) {
    const c = command[i];
    if (quote) {
      if (c === quote) quote = undefined;
      else if (quote === '"' && c === '\\' && i + 1 < command.length && '"\\$`'.includes(command[i + 1])) word += command[++i];
      else word += c;
    } else if (c === '"' || c === "'") { quote = c; open = true; }
    else if (c === '\\' && i + 1 < command.length) { word += command[++i]; open = true; }
    else if (/\s/.test(c)) { if (open || word) { words.push(word); word = ''; open = false; } }
    else word += c;
  }
  if (open || word) words.push(word);
  return words;
}

/** The first `name` on PATH that is a file this user may execute. */
export function findOnPath(name: string, environment: NodeJS.ProcessEnv): string | undefined {
  const names = process.platform === 'win32' ? [name + '.exe', name + '.cmd', name] : [name];
  for (const directory of (environment.PATH ?? environment.Path ?? '').split(path.delimiter)) {
    if (!directory || !path.isAbsolute(directory)) continue;
    for (const candidate of names) {
      const file = path.join(directory, candidate);
      try {
        if (!statSync(file).isFile()) continue;
        if (process.platform !== 'win32') accessSync(file, constants.X_OK);
        return file;
      } catch { /* Not here; keep looking. */ }
    }
  }
  return undefined;
}

export function setupConfigRoot(options: SetupOptions): string {
  const root = options.configRoot ?? path.join(os.homedir(), '.config', 'sudocode', 'acp', 'a2a');
  if (!path.isAbsolute(root)) throw new SetupStop('needs_input', '--config-root must be an absolute path', [
    'Pass --config-root with an absolute folder, or omit it to use the default under your home folder.']);
  return root;
}

function retry(options: SetupOptions, frontend: 'claude' | 'codex', extra: string[]): string[] {
  return [...(options.callerArgv ?? [process.execPath, options.entry, '--setup-' + frontend]), ...extra];
}

function nodeDataDirectory(options: SetupOptions, frontend: 'claude' | 'codex'): string {
  if (options.nodeDataDir !== undefined) {
    if (!path.isAbsolute(options.nodeDataDir) || !existsSync(options.nodeDataDir)) {
      throw new SetupStop('needs_input', '--node-data-dir must be an existing absolute folder', [
        'Pass --node-data-dir with the data folder of the Nexus node running on this machine.'],
      [{ why: 'Run again with the node data folder', argv: retry(options, frontend, ['--node-data-dir', '/ABSOLUTE/NODE/DATA']) }]);
    }
    return options.nodeDataDir;
  }
  const record = path.join(os.homedir(), '.nexus', 'run', 'node.json');
  let agents: unknown;
  try { agents = (JSON.parse(readFileSync(record, 'utf8')) as { agents_dir?: unknown }).agents_dir; } catch { /* Reported below. */ }
  if (typeof agents !== 'string' || !path.isAbsolute(agents) || !existsSync(path.dirname(agents))) {
    throw new SetupStop('needs_input', 'No Nexus node was found on this machine', [
      `There is no usable node record at ${record}.`,
      'Join this machine to the Nexus cluster first; that step belongs to the Nexus installation and is not done by this command.',
      'If a node is running but keeps its record elsewhere, pass --node-data-dir with its data folder.'],
    [{ why: 'Run again once the node exists, or name its data folder', argv: retry(options, frontend, ['--node-data-dir', '/ABSOLUTE/NODE/DATA']) }]);
  }
  return path.dirname(agents);
}

function enrollmentTool(options: SetupOptions, frontend: 'claude' | 'codex'): string[] {
  if (options.nexusEnrollArgv !== undefined) {
    const argv = options.nexusEnrollArgv;
    if (!Array.isArray(argv) || !argv.length || argv.some(arg => typeof arg !== 'string' || !arg) || !path.isAbsolute(argv[0])) {
      throw new SetupStop('needs_input', '--nexus-enroll-argv must be a JSON array starting with an absolute path', []);
    }
    return argv;
  }
  const name = process.platform === 'win32' ? 'nexus-agent-enroll.exe' : 'nexus-agent-enroll';
  let tool = options.nexusEnroll ?? findOnPath('nexus-agent-enroll', options.environment ?? process.env);
  if (tool === undefined) {
    const store = path.join(os.homedir(), '.local', 'share', 'nexus', 'operator');
    let newest = 0;
    try {
      for (const version of readdirSync(store)) {
        const candidate = path.join(store, version, name);
        try {
          const modified = statSync(candidate).mtimeMs;
          if (modified > newest) { newest = modified; tool = candidate; }
        } catch { /* Not a version folder with the tool. */ }
      }
    } catch { /* No store on this machine. */ }
  }
  if (tool === undefined || !path.isAbsolute(tool) || !existsSync(tool)) {
    throw new SetupStop('needs_input', 'The Nexus enrollment tool nexus-agent-enroll was not found', [
      'nexus-agent-enroll ships with the Nexus node release. It was not on PATH and not under ~/.local/share/nexus/operator/.',
      'Find it beside the nexusd-cluster executable of this machine\'s node, then pass its absolute path with --nexus-enroll.'],
    [{ why: 'Run again naming the enrollment tool', argv: retry(options, frontend, ['--nexus-enroll', '/ABSOLUTE/PATH/' + name]) }]);
  }
  return [tool];
}

function scodeExecutable(options: SetupOptions, frontend: 'claude' | 'codex'): string {
  const name = process.platform === 'win32' ? 'scode.exe' : 'scode';
  const shared = path.join(os.homedir(), '.nexus', 'bin', name);
  const scode = options.scode ?? (existsSync(shared) ? shared : findOnPath('scode', options.environment ?? process.env));
  if (scode === undefined || !path.isAbsolute(scode) || !existsSync(scode)) {
    throw new SetupStop('needs_input', 'scode was not found', [
      'Every attached agent runs an scode process, so scode must be installed on this machine.',
      'Install scode, or pass its absolute path with --scode.'],
    [{ why: 'Run again naming scode', argv: retry(options, frontend, ['--scode', '/ABSOLUTE/PATH/' + name]) }]);
  }
  return scode;
}

/** The enrollment grant and operator record both frontends share. Creates
 * what is missing, reuses what exists, and issues nothing. */
export async function prepareSharedSetup(options: SetupOptions, frontend: 'claude' | 'codex'): Promise<SharedSetup> {
  const root = setupConfigRoot(options);
  const journal = path.join(root, 'setup.jsonl');
  const operatorDirectory = path.join(root, 'operator');
  const grantFile = path.join(operatorDirectory, 'node-grant.json');
  const operatorFile = path.join(operatorDirectory, 'operator.json');
  const changed: string[] = [];
  const verified: string[] = [];

  if (existsSync(operatorFile) && existsSync(grantFile)) {
    const operator = JSON.parse(readFileSync(operatorFile, 'utf8')) as { nexusArgv?: unknown; scode?: { scodeArgv?: unknown } };
    const nexusArgv = operator.nexusArgv;
    const scodeArgv = operator.scode?.scodeArgv;
    if (!Array.isArray(nexusArgv) || nexusArgv.at(-1) !== grantFile || !Array.isArray(scodeArgv) || typeof scodeArgv[0] !== 'string') {
      throw new SetupStop('refused', 'The operator record in the config root is not one this command wrote', [
        `${operatorFile} exists but does not select ${grantFile}.`,
        'Move that file aside, or choose another folder with --config-root, then run again.']);
    }
    for (const [flag, wanted, current] of [['--scode', options.scode, scodeArgv[0]], ['--nexus-enroll', options.nexusEnroll, nexusArgv[0]]] as const) {
      if (wanted !== undefined && wanted !== current) {
        throw new SetupStop('refused', `${flag} differs from the existing operator record`, [
          `${operatorFile} already selects ${String(current)}; this command does not rewrite it.`,
          `To change it, move ${operatorFile} aside and run again. Agents that are already enrolled keep their saved profiles.`]);
      }
    }
    const issuer = (JSON.parse(readFileSync(grantFile, 'utf8')) as { endpoint?: unknown }).endpoint;
    if (options.issuer !== undefined && options.issuer !== issuer) {
      throw new SetupStop('refused', '--issuer differs from the existing enrollment grant', [
        `${grantFile} already names ${String(issuer)} as the issuer; this command does not rewrite it.`,
        'Omit --issuer to keep it, or choose another folder with --config-root for a different cluster.']);
    }
    verified.push(`enrollment grant ${grantFile} (issuer ${String(issuer)})`, `operator record ${operatorFile} (scode ${String(scodeArgv[0])})`);
    return { root, journal, enrollmentArgv: [process.execPath, options.entry, '--enroll-codex', '--config', operatorFile], changed, verified };
  }

  // Resolve every input before the first write, so a missing one changes nothing.
  const dataDirectory = nodeDataDirectory(options, frontend);
  const enroll = enrollmentTool(options, frontend);
  const scode = scodeExecutable(options, frontend);
  let issuer = options.issuer;
  if (issuer === undefined && existsSync(grantFile)) {
    issuer = String((JSON.parse(readFileSync(grantFile, 'utf8')) as { endpoint?: unknown }).endpoint);
  }
  if (issuer === undefined || !/^https:\/\/[^\s/]+:\d+$/.test(issuer)) {
    throw new SetupStop('needs_input', 'The issuer address is needed once on a new machine', [
      'Agent names are issued by one node of the cluster (its founder). This machine has no enrollment grant yet, so the address must be given once.',
      'It has the form https://HOST:2126. The Nexus node on this machine was started with a --peers argument that names the same host and port.',
      'Later runs read it from the grant and need no flag.'],
    [{ why: 'Run again with the issuer address', argv: retry(options, frontend, ['--issuer', 'https://HOST:2126']) }]);
  }

  mkdirSync(operatorDirectory, { recursive: true, mode: 0o700 });
  if (!existsSync(grantFile)) {
    const argv = [...enroll, '--data-dir', dataDirectory, '--initialize', '--grant', grantFile,
      '--endpoint', issuer, '--workspace-root', path.parse(os.homedir()).root];
    journalLine(journal, { frontend, step: 'grant', argv });
    try {
      await execute(argv[0], argv.slice(1), { env: options.environment ?? process.env, encoding: 'utf8', timeout: 30000, windowsHide: true });
    } catch (error) {
      const failed = error as { stderr?: unknown; message?: unknown };
      throw new Error(`The enrollment tool could not create the grant: ${String(failed.stderr || failed.message).trim()}`, { cause: error });
    }
    if (!existsSync(grantFile)) throw new Error(`The enrollment tool reported success but ${grantFile} does not exist`);
    changed.push(`created enrollment grant ${grantFile}`);
  }
  if (!existsSync(operatorFile)) {
    journalLine(journal, { frontend, step: 'operator', file: operatorFile });
    prepareCodexEnrollment(operatorFile, [...enroll, '--grant', grantFile],
      { scodeArgv: [scode, '--allowedTools', 'agent_list', '--allowedTools', 'send', 'acp'] }, options.entry);
    changed.push(`created operator record ${operatorFile}`);
  }
  verified.push(`enrollment grant ${grantFile} (issuer ${issuer})`, `operator record ${operatorFile} (scode ${scode})`);
  return { root, journal, enrollmentArgv: [process.execPath, options.entry, '--enroll-codex', '--config', operatorFile], changed, verified };
}

/** Turn whatever setup threw into the result the caller reads. */
export function setupFailure(frontend: 'claude' | 'codex', error: unknown, changed: string[], journal?: string): SetupResult {
  const stop = error instanceof SetupStop ? error : undefined;
  return {
    ok: false, frontend, state: stop?.state ?? 'failed', changed, verified: [], notVerified: [],
    instructions: stop?.instructions ?? [
      'Setup stopped on an unexpected error. Nothing is retried or rolled back automatically.',
      ...(changed.length ? ['The entries under "changed" were already applied and are safe to keep; running the same command again continues from there.'] : []),
      'Fix the cause named in "error", then run the same command again.'],
    next: stop?.next ?? [],
    changedAnything: changed.length > 0,
    ...(journal === undefined ? {} : { journal }),
    error: error instanceof Error ? error.message : String(error),
  };
}
