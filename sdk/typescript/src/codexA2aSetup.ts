import { execFile } from 'node:child_process';
import { accessSync, constants, existsSync, lstatSync, mkdirSync, readFileSync, readlinkSync, realpathSync, statSync, writeFileSync } from 'node:fs';
import * as net from 'node:net';
import * as os from 'node:os';
import * as path from 'node:path';
import { promisify } from 'node:util';
import { atomicWrite, journalLine, prepareSharedSetup, setupConfigRoot, setupFailure, SetupStop, shellWord, splitShellWords,
  type SetupOptions, type SetupResult, type SetupStep } from './a2aSetup';
import { codexMcpInstallationCommands, prepareCodexMcp, type CodexMcpConfig } from './codexMcp';
import { checkCodexNative } from './codexSetup';

const execute = promisify(execFile);
const SERVER = 'nexus_scode';
const TOOLS = ['scode_inbox', 'scode_send', 'scode_agent_list', 'scode_status'] as const;
const DEFAULT_PORT = 23262;

export interface CodexSetupOptions extends SetupOptions {
  codexCommand?: string;
  /** Test seam and advanced use: the native `codex` command as a full argv. */
  codexArgv?: string[];
  /** Loopback port of the per-user bridge owner (default 23262). */
  port?: number;
  /** Install a launcher at this `codex` path, for users who start Codex with `-c approvals_reviewer=user`. */
  commandEntry?: string;
  /** Let Codex call the bridge's four tools without asking. */
  approveTools?: boolean;
}

type Standing = 'absent' | 'current' | 'ours_other_sdk' | 'foreign';

function smallText(file: string): string | undefined {
  try {
    const metadata = statSync(file);
    if (!metadata.isFile() || metadata.size > 65536) return undefined;
    const bytes = readFileSync(file);
    return bytes.includes(0) ? undefined : bytes.toString('utf8');
  } catch { return undefined; }
}

/** `[node, entry, --launch-codex, --config, file, --]` when `file` is a launcher this package writes. */
function launcherWords(file: string): string[] | undefined {
  const line = smallText(file)?.split('\n').find(candidate => candidate.startsWith('exec '));
  if (!line) return undefined;
  const words = splitShellWords(line.slice(5));
  return words.length >= 6 && words[2] === '--launch-codex' && words[3] === '--config' && words[5] === '--' ? words : undefined;
}

function launcherScript(entry: string, configFile: string): string {
  return '#!/bin/sh\nexec ' + [process.execPath, entry, '--launch-codex', '--config', configFile, '--'].map(shellWord).join(' ') + ' "$@"\n';
}

/** The native Codex command behind `candidate`, as an argv no launcher of ours is part of. */
function nativeArgv(candidate: string): string[] {
  const real = realpathSync.native(candidate);
  return /\.[cm]?js$/.test(real) ? [process.execPath, real] : [real];
}

/** The first native `codex` on PATH, and the first launcher of ours that shadows it. */
function findCodex(environment: NodeJS.ProcessEnv): { argv?: string[]; launcher?: string } {
  let launcher: string | undefined;
  for (const directory of (environment.PATH ?? environment.Path ?? '').split(path.delimiter)) {
    if (!directory || !path.isAbsolute(directory)) continue;
    const candidate = path.join(directory, 'codex');
    try { if (!statSync(candidate).isFile()) continue; accessSync(candidate, constants.X_OK); } catch { continue; }
    if (launcherWords(candidate)) { launcher ??= candidate; continue; }
    return { argv: nativeArgv(candidate), launcher };
  }
  return { launcher };
}

function portInUse(port: number): Promise<boolean> {
  return new Promise(resolve => {
    const socket = net.createConnection({ host: '127.0.0.1', port });
    const done = (used: boolean) => { socket.destroy(); resolve(used); };
    socket.setTimeout(1000, () => done(false));
    socket.once('connect', () => done(true));
    socket.once('error', () => done(false));
  });
}

/** The tools of ours that Codex's config already lets it call without asking. */
function approvedTools(text: string): Set<string> {
  const approved = new Set<string>();
  let tool: string | undefined;
  for (const line of text.split(/\r?\n/)) {
    const header = /^\s*\[([^\]]+)\]\s*$/.exec(line);
    if (header) { tool = /^mcp_servers\.nexus_scode\.tools\.([A-Za-z0-9_]+)$/.exec(header[1].trim())?.[1]; continue; }
    if (tool && /^\s*approval_mode\s*=\s*"approve"\s*$/.test(line)) approved.add(tool);
  }
  return approved;
}

/** One command that attaches Codex to the agent network: grant and operator
 * record if absent, the bridge config, Codex's background server, the global
 * MCP entry and, when asked, a launcher and tool approvals. Each is read back.
 * Safe to run again at any time. */
export async function setupCodex(options: CodexSetupOptions): Promise<SetupResult> {
  const changed: string[] = [];
  let journal: string | undefined;
  try {
    const environment = options.environment ?? process.env;
    const callerArgv = options.callerArgv ?? [process.execPath, options.entry, '--setup-codex'];
    if (!path.isAbsolute(options.entry) || !existsSync(options.entry)) throw new Error('The bridge entry must be an existing absolute path');
    if (process.platform === 'win32') {
      throw new SetupStop('refused', 'One-command Codex setup does not cover Windows yet', [
        'On Windows the bridge owner runs as a Task Scheduler task, which this command does not prepare.',
        'Use the documented steps instead: --init-codex-operator, --init-codex and --install-codex (see the package README, "Global native Codex attachment").']);
    }
    const root = setupConfigRoot(options);
    journal = path.join(root, 'setup.jsonl');
    const codexDirectory = path.join(root, 'codex');
    const configFile = path.join(codexDirectory, 'runtime.json');
    const node = process.execPath;
    const run = async (argv: string[], record = true): Promise<{ stdout: string; stderr: string }> => {
      if (record) journalLine(journal!, { frontend: 'codex', step: 'codex-cli', argv });
      return execute(argv[0], argv.slice(1), { env: environment, encoding: 'utf8', timeout: 60000, windowsHide: true, maxBuffer: 1 << 20 });
    };

    // The native command, never a launcher of ours: the launcher runs it.
    const found = findCodex(environment);
    let codex = options.codexArgv;
    if (codex === undefined && options.codexCommand !== undefined && path.isAbsolute(options.codexCommand)
        && existsSync(options.codexCommand) && !launcherWords(options.codexCommand)) codex = nativeArgv(options.codexCommand);
    codex ??= found.argv;
    if (!codex || !codex.length || !path.isAbsolute(codex[0])) {
      throw new SetupStop('needs_input', 'The codex command was not found', [
        'The Codex command-line program "codex" was not found on PATH.',
        'Install Codex and sign in once, or pass the absolute path of its real executable (not a launcher) with --codex-command.'],
      [{ why: 'Run again naming the codex executable', argv: [...callerArgv, '--codex-command', '/ABSOLUTE/PATH/codex'] }]);
    }
    const codexHome = environment.CODEX_HOME || path.join(os.homedir(), '.codex');
    if (!path.isAbsolute(codexHome) || !existsSync(codexHome) || !statSync(codexHome).isDirectory()) {
      throw new SetupStop('needs_input', 'Codex has not been used on this machine yet', [
        `Codex keeps its settings in ${codexHome}, which does not exist.`,
        'Start Codex once and sign in, then run this command again.'],
      [{ why: 'Then run setup again', argv: callerArgv }]);
    }
    const codexConfig = path.join(codexHome, 'config.toml');

    // Look before writing anything: an attachment this command did not make is never edited.
    const registration = async (): Promise<Standing> => {
      let text: string;
      try { text = (await run([...codex!, 'mcp', 'get', SERVER, '--json'], false)).stdout; } catch (error) {
        const failed = error as { code?: unknown; stderr?: unknown };
        if (failed.code === 1 && /No MCP server named ['"]nexus_scode['"] found/.test(String(failed.stderr))) return 'absent';
        throw new Error(`Could not read Codex's MCP registration: ${String(failed.stderr || error).trim()}`, { cause: error });
      }
      const entry = JSON.parse(text) as { enabled?: unknown; transport?: { type?: unknown; command?: unknown; args?: unknown; env?: unknown; cwd?: unknown } };
      const transport = entry.transport;
      const args = Array.isArray(transport?.args) ? transport.args as unknown[] : [];
      if (transport?.type !== 'stdio' || args.length !== 4 || args[1] !== '--codex' || args[2] !== '--config' || args[3] !== configFile) return 'foreign';
      const plain = (transport.env == null || !Object.keys(transport.env as object).length) && transport.cwd == null && entry.enabled !== false;
      return transport.command === node && args[0] === options.entry && plain ? 'current' : 'ours_other_sdk';
    };
    const launcherStanding = (file: string | undefined): Standing => {
      const words = file ? launcherWords(file) : undefined;
      if (!words) return 'absent';
      if (words[4] !== configFile) return 'foreign';
      return words[0] === node && words[1] === options.entry ? 'current' : 'ours_other_sdk';
    };
    const registered = await registration();
    const launcher = options.commandEntry ?? found.launcher;
    const launched = launcherStanding(launcher);
    if (registered === 'foreign' || launched === 'foreign') {
      throw new SetupStop('refused', 'Codex already has a different A2A attachment', [
        'Codex is already attached to the agent network by something this command did not install'
          + (registered === 'foreign' ? `: the "${SERVER}" entry in ${codexConfig}` : '')
          + (launched === 'foreign' ? `; the codex launcher at ${launcher}` : '') + '.',
        'This command never edits another attachment. To replace it, do these things in order, then run this command again:',
        '1. Close every Codex session and wait about two minutes, until the old bridge has stopped by itself.',
        '2. Remove the old entry with the first command under "next".',
        ...(launched === 'foreign' ? [`3. Put the original codex command back at ${launcher}, or delete that file if another codex is on PATH.`] : []),
        'Agents enrolled through the old attachment do not carry over: each conversation enrolls a new name the first time it starts.'],
      [{ why: 'Remove the old registration', argv: [...codex, 'mcp', 'remove', SERVER] }, { why: 'Then run setup again', argv: callerArgv }]);
    }

    // The bridge's owner serves every open session. Nothing of it is replaced while it runs.
    const existing = existsSync(configFile) ? JSON.parse(readFileSync(configFile, 'utf8')) as CodexMcpConfig : undefined;
    if (existing && (existing.mode !== 'codex-autojoin' || existing.stateDirectory !== path.join(codexDirectory, 'codex-agents'))) {
      throw new SetupStop('refused', 'The Codex bridge config in the config root is not one this command wrote', [
        `${configFile} exists but does not keep its state under ${codexDirectory}.`,
        'Move that file aside, or choose another folder with --config-root, then run again.']);
    }
    if (existing && options.port !== undefined && options.port !== existing.endpoint.port) {
      throw new SetupStop('refused', '--port differs from the existing bridge config', [
        `${configFile} already uses port ${existing.endpoint.port}; this command does not rewrite it.`, 'Omit --port to keep it.']);
    }
    const port = existing?.endpoint.port ?? options.port ?? DEFAULT_PORT;
    if (!Number.isInteger(port) || port < 1024 || port > 65535) {
      throw new SetupStop('needs_input', '--port must be a number from 1024 to 65535', []);
    }
    const moving = registered === 'ours_other_sdk' || launched === 'ours_other_sdk';
    if ((moving || registered === 'absent') && await portInUse(port)) {
      throw moving ? new SetupStop('refused', 'Codex sessions are still open on the old bridge', [
        'The bridge that serves the open Codex sessions is still running, and it cannot be replaced while any session uses it.',
        'Close every Codex session. Wait about two minutes: Codex unloads the conversations and the bridge stops by itself.',
        'Then run this command again, and resume the conversations afterwards. They keep their agent names.'],
      [{ why: 'Run again once every Codex session is closed', argv: callerArgv }])
        : new SetupStop('needs_input', `Port ${port} on this machine is already in use`, [
          `The bridge needs a free local port and ${port} is taken by another program.`, 'Pass another port with --port.'],
        [{ why: 'Run again with a free port', argv: [...callerArgv, '--port', String(port + 1)] }]);
    }
    if (options.commandEntry !== undefined) {
      const target = options.commandEntry;
      let there: ReturnType<typeof lstatSync> | undefined;
      try { there = lstatSync(target); } catch { /* Absent. */ }
      if (!path.isAbsolute(target) || path.basename(target) !== 'codex' || !existsSync(path.dirname(target))
          || codex.some(arg => path.isAbsolute(arg) && path.resolve(arg) === path.resolve(target))
          || (there && !there.isSymbolicLink() && smallText(target) === undefined)) {
        throw new SetupStop('needs_input', '--command-entry must be a replaceable "codex" path', [
          '--command-entry names where the launcher is written. It must be an absolute path ending in "codex", in an existing folder that comes before the real codex on PATH.',
          'It may be absent, a symbolic link or a small script. A real program is never replaced.']);
      }
    }

    const shared = await prepareSharedSetup(options, 'codex');
    changed.push(...shared.changed);

    // The bridge config. Its folder keeps every conversation's identity, so it is never recreated.
    if (!existing) {
      journalLine(journal, { frontend: 'codex', step: 'config', file: configFile });
      prepareCodexMcp(configFile, codex, shared.enrollmentArgv, [], options.entry, port, 'node-namespace', undefined, 3);
      changed.push(`created bridge config ${configFile}`);
    } else if (JSON.stringify(existing.native.codexArgv) !== JSON.stringify(codex)
        || JSON.stringify(existing.enrollmentArgv) !== JSON.stringify(shared.enrollmentArgv)) {
      journalLine(journal, { frontend: 'codex', step: 'config', file: configFile, replaced: true });
      atomicWrite(configFile, JSON.stringify({ ...existing, native: { ...existing.native, codexArgv: codex },
        enrollmentArgv: shared.enrollmentArgv }, null, 2) + '\n', 0o600);
      changed.push(`updated bridge config ${configFile}`);
    }
    const commands = codexMcpInstallationCommands(configFile, options.entry);

    // Codex's own background server is what the bridge reads the open conversations from.
    const running = async (): Promise<boolean> => {
      try { return (JSON.parse((await run([...codex!, 'app-server', 'daemon', 'version'], false)).stdout) as { status?: unknown }).status === 'running'; }
      catch { return false; }
    };
    if (!await running()) {
      await run(commands.nativePreparationArgv);
      if (!await running()) throw new Error('Codex reported that its background server did not start ("codex app-server daemon version" is not "running")');
      changed.push('started Codex\'s background server (codex app-server daemon start)');
    }
    await checkCodexNative(configFile, options.entry);

    // The global MCP entry. `codex mcp add` replaces an entry and drops its tool approvals, so those are kept by hand.
    const tomlBefore = existsSync(codexConfig) ? readFileSync(codexConfig, 'utf8') : '';
    const kept = approvedTools(tomlBefore);
    if (registered !== 'current') {
      await run(commands.registrationArgv);
      if (await registration() !== 'current') throw new Error(`The "${SERVER}" entry read back from Codex is not the one this command registers`);
      changed.push(`${registered === 'absent' ? 'registered' : 'moved'} the "${SERVER}" MCP entry in ${codexConfig}`);
    }
    const wanted = new Set<string>([...kept].filter(tool => (TOOLS as readonly string[]).includes(tool)));
    if (options.approveTools) for (const tool of TOOLS) wanted.add(tool);
    const toml = existsSync(codexConfig) ? readFileSync(codexConfig, 'utf8') : '';
    const missing = [...wanted].filter(tool => !new RegExp(`^\\s*\\[mcp_servers\\.nexus_scode\\.tools\\.${tool}\\]\\s*$`, 'm').test(toml)).sort();
    if (missing.length) {
      const backups = path.join(codexDirectory, 'backups');
      mkdirSync(backups, { recursive: true, mode: 0o700 });
      const backup = path.join(backups, `config-${new Date().toISOString().replace(/[:.]/g, '')}.toml`);
      writeFileSync(backup, tomlBefore, { flag: 'wx', mode: 0o600 });
      journalLine(journal, { frontend: 'codex', step: 'approvals', file: codexConfig, tools: missing, backup });
      atomicWrite(codexConfig, toml + (toml === '' || toml.endsWith('\n') ? '' : '\n')
        + missing.map(tool => `\n[mcp_servers.nexus_scode.tools.${tool}]\napproval_mode = "approve"\n`).join(''),
      existsSync(codexConfig) ? lstatSync(codexConfig).mode & 0o777 : 0o600);
      if (await registration() !== 'current') throw new Error(`Codex no longer reads its "${SERVER}" entry after the tool approvals were written; restore ${backup}`);
      const newly = missing.filter(tool => !kept.has(tool));
      if (newly.length) changed.push(`allowed Codex to call ${newly.join(', ')} without asking, in ${codexConfig} (previous copy: ${backup})`);
    }
    const approved = approvedTools(existsSync(codexConfig) ? readFileSync(codexConfig, 'utf8') : '');
    const unapproved = TOOLS.filter(tool => !approved.has(tool));

    // The launcher: only one of ours is moved, and a new one is written only where the caller named it.
    if (launcher !== undefined && launched !== 'current' && (launched === 'ours_other_sdk' || options.commandEntry !== undefined)) {
      let previous: string | null = null;
      try { previous = lstatSync(launcher).isSymbolicLink() ? 'symlink to ' + readlinkSync(launcher) : readFileSync(launcher, 'utf8'); } catch { /* Absent. */ }
      journalLine(journal, { frontend: 'codex', step: 'launcher', file: launcher, previous });
      atomicWrite(launcher, launcherScript(options.entry, configFile), 0o755);
      if (launcherStanding(launcher) !== 'current') throw new Error(`The launcher read back from ${launcher} is not the one this command writes`);
      changed.push(`${launched === 'ours_other_sdk' ? 'moved' : 'installed'} the codex launcher at ${launcher}`
        + (previous !== null && launched !== 'ours_other_sdk' ? ' (what was there is recorded in the journal)' : ''));
    }
    const hasLauncher = launcherStanding(launcher) === 'current';

    const state = !changed.length ? 'already_installed' : moving ? 'moved_to_this_sdk' : 'installed';
    journalLine(journal, { frontend: 'codex', step: 'done', state, entry: options.entry });
    const next: SetupStep[] = [];
    const instructions: string[] = state === 'already_installed' ? ['Codex is already attached through this SDK; nothing was changed.'] : [
      'Close every Codex session that is open now, then start or resume them. A session opened before this setup does not have the bridge\'s tools.'];
    instructions.push(
      'In a Codex session, call the tool scode_agent_list. A list of agents means the session has joined. An answer of "not_submitted" names what is still missing, and the bridge keeps retrying by itself.',
      'Other agents then see the session in their agent list as "Codex · <machine> · <folder>".');
    if (unapproved.length) {
      instructions.push(`Codex asks before it calls ${unapproved.join(', ')}. Until those are allowed, a message cannot wake an idle session: choose "always allow" the first time each is asked, or run the command under "next".`);
      next.push({ why: 'Allow the bridge\'s four tools now, so messages can wake an idle session', argv: [...callerArgv.filter(arg => arg !== '--approve-tools'), '--approve-tools'] });
    }
    if (!hasLauncher) {
      instructions.push('If you start Codex with "-c approvals_reviewer=user", Codex runs a private server the bridge cannot see. Start it without that option, or run this command again with --command-entry PATH to install a launcher that removes it.');
    }
    if (state !== 'already_installed') instructions.push('Running this same command again is safe and reports the current state.');
    return {
      ok: true, frontend: 'codex', state, changed,
      verified: [...shared.verified, `bridge config ${configFile} (port ${port})`,
        `Codex's background server is running and answers the bridge (Codex home ${codexHome})`,
        `the "${SERVER}" MCP entry, read back through codex mcp get`,
        ...(hasLauncher ? [`the codex launcher at ${launcher}`] : []),
        ...(approved.size ? [`Codex may call ${[...approved].sort().join(', ')} without asking`] : [])],
      notVerified: ['No Codex session was started and no agent name was issued by this command.'],
      instructions, next, journal,
    };
  } catch (error) {
    if (journal && changed.length) { try { journalLine(journal, { frontend: 'codex', step: 'stopped', error: String(error) }); } catch { /* Report the original error. */ } }
    return setupFailure('codex', error, changed, journal);
  }
}
