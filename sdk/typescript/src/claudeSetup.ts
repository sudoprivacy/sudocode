import { execFile } from 'node:child_process';
import { createHash, randomUUID } from 'node:crypto';
import { closeSync, existsSync, fsyncSync, lstatSync, mkdirSync, openSync, readFileSync, renameSync, unlinkSync, writeFileSync } from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { promisify } from 'node:util';
import { findOnPath, journalLine, prepareSharedSetup, setupConfigRoot, setupFailure, SetupStop,
  type SetupOptions, type SetupResult } from './a2aSetup';
import { validateClaudeAutojoinConfig, type ClaudeAutojoinConfig } from './claudeAutojoin';

const execute = promisify(execFile);
const SERVER = 'nexus_scode';
const HOOK_EVENTS = ['SessionStart', 'CwdChanged', 'FileChanged'] as const;
type HookEvent = typeof HOOK_EVENTS[number];
const HOOK_FLAG: Record<HookEvent, '--claude-watch' | '--claude-deliver'> = {
  SessionStart: '--claude-watch', CwdChanged: '--claude-watch', FileChanged: '--claude-deliver' };

export interface ClaudeSetupOptions extends SetupOptions {
  claudeCommand?: string;
  /** Test seam and advanced use: the `claude` command as a full argv. */
  claudeArgv?: string[];
}

interface HookCommand { type?: unknown; command?: unknown; timeout?: unknown; asyncRewake?: unknown }
interface HookGroup { hooks?: HookCommand[] }
type Settings = { hooks?: Record<string, HookGroup[]> } & Record<string, unknown>;
type Standing = 'current' | 'ours_other_sdk' | 'foreign';

/** Where Claude Code keeps its user-scope configuration. */
export function claudeUserFiles(environment: NodeJS.ProcessEnv = process.env): { global: string; settings: string } {
  const directory = environment.CLAUDE_CONFIG_DIR;
  if (directory) {
    if (!path.isAbsolute(directory)) throw new Error('CLAUDE_CONFIG_DIR must be an absolute path');
    return { global: path.join(directory, '.claude.json'), settings: path.join(directory, 'settings.json') };
  }
  return { global: path.join(os.homedir(), '.claude.json'), settings: path.join(os.homedir(), '.claude', 'settings.json') };
}

/** Quote for the shell Claude Code runs hooks in; plain paths stay readable. */
function shellWord(value: string): string {
  return /^[A-Za-z0-9_@%+=:,./-]+$/.test(value) ? value : "'" + value.replace(/'/g, "'\\''") + "'";
}

/** Split a hook command the way a POSIX shell would for quotes and blanks. */
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

function readJson<T>(file: string): { value: T; bytes: Buffer } | undefined {
  let bytes: Buffer;
  try { bytes = readFileSync(file); } catch (error) {
    if ((error as NodeJS.ErrnoException).code === 'ENOENT') return undefined;
    throw error;
  }
  try { return { value: JSON.parse(bytes.toString('utf8')) as T, bytes }; } catch (error) {
    throw new SetupStop('refused', `${file} is not valid JSON`, [
      `Claude Code's file ${file} could not be parsed, so this command will not edit it.`,
      'Open it, fix or restore it, then run this command again.']);
  }
}

function registrationStanding(entry: unknown, expected: { command: string; args: string[] }, configFile: string): Standing | 'absent' {
  if (entry === undefined || entry === null) return 'absent';
  const candidate = entry as { type?: unknown; command?: unknown; args?: unknown; env?: unknown };
  const args = Array.isArray(candidate.args) ? candidate.args as unknown[] : [];
  const emptyEnvironment = candidate.env == null || (typeof candidate.env === 'object' && !Object.keys(candidate.env as object).length);
  const ours = (candidate.type === undefined || candidate.type === 'stdio') && typeof candidate.command === 'string'
    && args.length === 4 && args[1] === '--claude' && args[2] === '--config' && args[3] === configFile;
  if (!ours) return 'foreign';
  return candidate.command === expected.command && args[0] === expected.args[0] && emptyEnvironment ? 'current' : 'ours_other_sdk';
}

/** Every bridge hook in the settings, with how it stands against this install. */
function bridgeHooks(settings: Settings, node: string, entry: string, configFile: string):
  Array<{ event: string; group: number; index: number; standing: Standing }> {
  const found: Array<{ event: string; group: number; index: number; standing: Standing }> = [];
  for (const [event, groups] of Object.entries(settings.hooks ?? {})) {
    if (!Array.isArray(groups)) continue;
    groups.forEach((group, groupIndex) => (Array.isArray(group?.hooks) ? group.hooks : []).forEach((hook, index) => {
      if (typeof hook?.command !== 'string') return;
      const words = splitShellWords(hook.command);
      const flag = words.find(word => word === '--claude-watch' || word === '--claude-deliver');
      if (!flag) return;
      const ours = words.length === 5 && words[2] === flag && words[3] === '--config' && words[4] === configFile
        && HOOK_FLAG[event as HookEvent] === flag;
      const exact = ours && words[0] === node && words[1] === entry && hook.type === 'command'
        && (flag !== '--claude-deliver' || hook.asyncRewake === true);
      found.push({ event, group: groupIndex, index, standing: exact ? 'current' : ours ? 'ours_other_sdk' : 'foreign' });
    }));
  }
  return found;
}

function atomicWrite(file: string, bytes: Buffer | string, mode: number): void {
  const temporary = path.join(path.dirname(file), `.${path.basename(file)}.${randomUUID()}`);
  const handle = openSync(temporary, 'wx', mode);
  try { writeFileSync(handle, bytes); fsyncSync(handle); } finally { closeSync(handle); }
  try { renameSync(temporary, file); } catch (error) { try { unlinkSync(temporary); } catch { /* Best effort. */ } throw error; }
}

/** One command that attaches Claude Code to the agent network: grant and
 * operator record if absent, the bridge config, the user-scope MCP entry and
 * the three hooks, each read back. Safe to run again at any time. */
export async function setupClaude(options: ClaudeSetupOptions): Promise<SetupResult> {
  const changed: string[] = [];
  let journal: string | undefined;
  try {
    const environment = options.environment ?? process.env;
    const callerArgv = options.callerArgv ?? [process.execPath, options.entry, '--setup-claude'];
    if (!path.isAbsolute(options.entry) || !existsSync(options.entry)) throw new Error('The bridge entry must be an existing absolute path');
    const root = setupConfigRoot(options);
    journal = path.join(root, 'setup.jsonl');
    const files = claudeUserFiles(environment);
    const claudeDirectory = path.join(root, 'claude');
    const configFile = path.join(claudeDirectory, 'autojoin.json');
    const node = process.execPath;
    const expected = { type: 'stdio', command: node, args: [options.entry, '--claude', '--config', configFile], env: {} };

    let claude = options.claudeArgv;
    if (claude === undefined) {
      const command = options.claudeCommand ?? findOnPath('claude', environment);
      if (command === undefined || !path.isAbsolute(command) || !existsSync(command) || /\.(cmd|bat)$/i.test(command)) {
        throw new SetupStop('needs_input', 'The claude command was not found', [
          'Claude Code\'s command-line program "claude" was not found on PATH as a directly runnable file.',
          'Install Claude Code first, or pass the absolute path of its executable with --claude-command.'],
        [{ why: 'Run again naming the claude executable', argv: [...callerArgv, '--claude-command', '/ABSOLUTE/PATH/claude'] }]);
      }
      claude = [command];
    }

    // Look before writing anything: an attachment this command did not make is never edited.
    const before = readJson<{ mcpServers?: Record<string, unknown> }>(files.global);
    const settingsBefore = readJson<Settings>(files.settings);
    const registered = registrationStanding(before?.value.mcpServers?.[SERVER], expected, configFile);
    const hooksBefore = bridgeHooks(settingsBefore?.value ?? {}, node, options.entry, configFile);
    if (registered === 'foreign' || hooksBefore.some(hook => hook.standing === 'foreign')) {
      throw new SetupStop('refused', 'Claude Code already has a different A2A attachment', [
        'Claude Code is already attached to the agent network by something this command did not install'
          + (registered === 'foreign' ? `: the "${SERVER}" entry in ${files.global}` : '')
          + (hooksBefore.some(hook => hook.standing === 'foreign') ? `; hooks with --claude-watch or --claude-deliver in ${files.settings}` : '') + '.',
        'This command never edits another attachment. To replace it, do these three things, then run this command again:',
        `1. Remove the old entry with the first command under "next".`,
        `2. In ${files.settings}, delete every hook entry whose command contains --claude-watch or --claude-deliver.`,
        '3. Restart the Claude Code sessions that should join through the new attachment.',
        'Agents enrolled through the old attachment do not carry over: each session enrolls a new name the first time it starts.'],
      [{ why: 'Remove the old registration', argv: [...claude, 'mcp', 'remove', SERVER, '-s', 'user'] },
        { why: 'Then run setup again', argv: callerArgv }]);
    }

    const shared = await prepareSharedSetup(options, 'claude');
    changed.push(...shared.changed);
    const moved = registered === 'ours_other_sdk' || hooksBefore.some(hook => hook.standing === 'ours_other_sdk');

    // The bridge config. Its folder keeps every session's identity, so it is never recreated.
    const wanted: ClaudeAutojoinConfig = validateClaudeAutojoinConfig({ version: 1, mode: 'claude-autojoin',
      stateDirectory: path.join(claudeDirectory, 'sessions'), auditLog: path.join(claudeDirectory, 'network.jsonl'),
      enrollmentArgv: shared.enrollmentArgv, peerPolicy: 'node-namespace' });
    const current = readJson<ClaudeAutojoinConfig>(configFile);
    if (!current || JSON.stringify(current.value) !== JSON.stringify(wanted)) {
      mkdirSync(claudeDirectory, { recursive: true, mode: 0o700 });
      journalLine(journal, { frontend: 'claude', step: 'config', file: configFile, replaced: Boolean(current) });
      atomicWrite(configFile, JSON.stringify(wanted, null, 2) + '\n', 0o600);
      changed.push(`${current ? 'updated' : 'created'} bridge config ${configFile}`);
    }

    // The MCP entry, through Claude Code's own command so its file locking applies.
    if (registered !== 'current') {
      const run = async (argv: string[]): Promise<void> => {
        journalLine(journal!, { frontend: 'claude', step: 'claude-cli', argv });
        try {
          await execute(argv[0], argv.slice(1), { env: environment, encoding: 'utf8', timeout: 60000, windowsHide: true });
        } catch (error) {
          const failed = error as { stderr?: unknown; message?: unknown };
          throw new Error(`"${argv.slice(0, 4).join(' ')} …" failed: ${String(failed.stderr || failed.message).trim()}`, { cause: error });
        }
      };
      if (registered === 'ours_other_sdk') await run([...claude, 'mcp', 'remove', SERVER, '-s', 'user']);
      await run([...claude, 'mcp', 'add-json', SERVER, JSON.stringify(expected), '-s', 'user']);
    }
    // Claude Code's command exits 0 even when it keeps an existing entry, so only the file is proof.
    const after = readJson<{ mcpServers?: Record<string, unknown> }>(files.global);
    if (registrationStanding(after?.value.mcpServers?.[SERVER], expected, configFile) !== 'current') {
      throw new Error(`The "${SERVER}" entry read back from ${files.global} is not the one this command registers`);
    }
    if (registered !== 'current') changed.push(`${registered === 'absent' ? 'registered' : 'moved'} the "${SERVER}" MCP entry in ${files.global}`);

    // The three hooks, merged into whatever the user already has.
    const latest = readJson<Settings>(files.settings);
    const settings: Settings = latest ? structuredClone(latest.value) : {};
    if (typeof settings !== 'object' || settings === null || Array.isArray(settings)) throw new Error(`${files.settings} is not a JSON object`);
    const stale = bridgeHooks(settings, node, options.entry, configFile).filter(hook => hook.standing !== 'current');
    if (stale.some(hook => hook.standing === 'foreign')) throw new Error(`${files.settings} gained another attachment's hooks during setup`);
    for (const hook of stale.sort((a, b) => b.group - a.group || b.index - a.index)) {
      const groups = settings.hooks![hook.event];
      groups[hook.group].hooks!.splice(hook.index, 1);
      if (!groups[hook.group].hooks!.length) groups.splice(hook.group, 1);
    }
    const present = new Set(bridgeHooks(settings, node, options.entry, configFile).map(hook => hook.event));
    for (const event of HOOK_EVENTS) {
      if (present.has(event)) continue;
      const flag = HOOK_FLAG[event];
      const command = [node, options.entry].map(shellWord).join(' ') + ` ${flag} --config ${shellWord(configFile)}`;
      settings.hooks ??= {};
      (settings.hooks[event] ??= []).push({ hooks: [flag === '--claude-deliver'
        ? { type: 'command', command, timeout: 60, asyncRewake: true } : { type: 'command', command, timeout: 30 }] });
    }
    if (!latest || JSON.stringify(latest.value) !== JSON.stringify(settings)) {
      const text = latest?.bytes.toString('utf8');
      const indent = text?.match(/^\{\r?\n([ \t]+)"/)?.[1] ?? '  ';
      mkdirSync(path.dirname(files.settings), { recursive: true, mode: 0o700 });
      let backup: string | undefined;
      if (latest) {
        backup = path.join(claudeDirectory, 'backups', `settings-${new Date().toISOString().replace(/[:.]/g, '')}-${createHash('sha256').update(latest.bytes).digest('hex').slice(0, 8)}.json`);
        mkdirSync(path.dirname(backup), { recursive: true, mode: 0o700 });
        writeFileSync(backup, latest.bytes, { flag: 'wx', mode: 0o600 });
      }
      journalLine(journal, { frontend: 'claude', step: 'hooks', file: files.settings, backup: backup ?? null });
      // The file is the user's: stop if it changed since it was read, and keep its mode and indentation.
      const now = readJson<Settings>(files.settings);
      if (Boolean(now) !== Boolean(latest) || (now && latest && !now.bytes.equals(latest.bytes))) {
        throw new Error(`${files.settings} changed while setup was running; nothing was written to it. Run the same command again.`);
      }
      atomicWrite(files.settings, JSON.stringify(settings, null, indent) + (text === undefined || /\n$/.test(text) ? '\n' : ''),
        latest ? lstatSync(files.settings).mode & 0o777 : 0o600);
      changed.push(`${latest ? 'added the three A2A hooks to' : 'created'} ${files.settings}` + (backup ? ` (previous copy: ${backup})` : ''));
    }
    const hooksAfter = bridgeHooks(readJson<Settings>(files.settings)?.value ?? {}, node, options.entry, configFile);
    if (HOOK_EVENTS.some(event => !hooksAfter.some(hook => hook.event === event && hook.standing === 'current'))
        || hooksAfter.some(hook => hook.standing !== 'current')) {
      throw new Error(`The hooks read back from ${files.settings} are not the three this command installs`);
    }

    const state = !changed.length ? 'already_installed' : moved ? 'moved_to_this_sdk' : 'installed';
    journalLine(journal, { frontend: 'claude', step: 'done', state, entry: options.entry });
    return {
      ok: true, frontend: 'claude', state, changed,
      verified: [...shared.verified, `bridge config ${configFile}`,
        `the "${SERVER}" MCP entry, read back from ${files.global}`,
        `hooks SessionStart, CwdChanged and FileChanged (with asyncRewake), read back from ${files.settings}`],
      notVerified: ['No Claude Code session was started and no agent name was issued by this command.'],
      instructions: state === 'already_installed' ? [
        'Claude Code is already attached through this SDK; nothing was changed.',
        'To check a live session, call the tool scode_status inside Claude Code: it should report connected and an agent name starting with "claude-".'] : [
        'Start a new Claude Code session in any project folder. Sessions that are already open keep what they started with until they are restarted.',
        'Inside that session, call the tool scode_status. It should report connected and an agent name starting with "claude-".',
        'Each new session asks the issuer for an agent name the first time it starts (a resumed session keeps its name), so the Nexus node on this machine must be running and able to reach the issuer.',
        'If Claude Code asks whether to allow the scode_ tools, allow them.',
        'Running this same command again is safe and reports the current state.'],
      next: [], journal,
    };
  } catch (error) {
    if (journal && changed.length) { try { journalLine(journal, { frontend: 'claude', step: 'stopped', error: String(error) }); } catch { /* Report the original error. */ } }
    return setupFailure('claude', error, changed, journal);
  }
}
