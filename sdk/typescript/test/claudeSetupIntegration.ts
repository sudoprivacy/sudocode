import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { once } from 'node:events';

const root = path.resolve(__dirname, '../..');
const { splitShellWords } = require(path.join(root, 'dist/index.cjs')) as typeof import('../src/index');
const server = path.join(root, 'dist/server.cjs');
const claudeArgv = [process.execPath, path.join(root, 'test/fixtures/claude-setup-cli.mjs')];
const enrollArgv = [process.execPath, path.join(root, 'test/fixtures/nexus-enroll-initialize.mjs')];
const posix = process.platform !== 'win32';
const digest = (file: string) => createHash('sha256').update(fs.readFileSync(file)).digest('hex');

interface Result {
  ok: boolean; frontend: string; state: string; changed: string[]; verified: string[]; notVerified: string[];
  instructions: string[]; next: Array<{ why: string; argv: string[] }>; changedAnything?: boolean; journal?: string; error?: string;
}

interface Home { home: string; claude: string; configRoot: string; env: Record<string, string>; global: string; settings: string; log: string }

/** A private home with its own Claude config folder. The config root has a
 * blank in its name so that every written command line must be quoted. */
function home(base: string, name: string, node = true): Home {
  const dir = path.join(base, name);
  const claude = path.join(dir, 'claude config');
  const configRoot = path.join(dir, 'a2a root');
  fs.mkdirSync(claude, { recursive: true, mode: 0o700 });
  if (node) {
    fs.mkdirSync(path.join(dir, 'node-data', 'agents'), { recursive: true });
    fs.mkdirSync(path.join(dir, '.nexus', 'run'), { recursive: true });
    fs.writeFileSync(path.join(dir, '.nexus', 'run', 'node.json'),
      JSON.stringify({ version: 1, endpoint: '127.0.0.1:2126', agents_dir: path.join(dir, 'node-data', 'agents') }));
  }
  const env: Record<string, string> = { HOME: dir, USERPROFILE: dir, CLAUDE_CONFIG_DIR: claude };
  for (const key of ['PATH', 'Path', 'SystemRoot', 'TEMP', 'TMP', 'TMPDIR']) if (process.env[key]) env[key] = process.env[key]!;
  return { home: dir, claude, configRoot, env, global: path.join(claude, '.claude.json'),
    settings: path.join(claude, 'settings.json'), log: path.join(claude, 'claude-cli.log') };
}

async function setup(target: Home, extra: string[] = [], options: { entry?: string; env?: Record<string, string>; seams?: boolean } = {}):
  Promise<{ code: number | null; result: Result }> {
  const argv = [options.entry ?? server, '--setup-claude', '--config-root', target.configRoot,
    ...(options.seams === false ? [] : ['--claude-argv', JSON.stringify(claudeArgv), '--nexus-enroll-argv', JSON.stringify(enrollArgv),
      '--scode', process.execPath]), ...extra];
  const child = spawn(process.execPath, argv, { env: { ...target.env, ...options.env }, shell: false, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] });
  let stdout = '', stderr = '';
  child.stdout.on('data', bytes => stdout += String(bytes));
  child.stderr.on('data', bytes => stderr += String(bytes));
  const [code] = await once(child, 'close');
  let result: Result;
  try { result = JSON.parse(stdout) as Result; } catch { throw new Error(`setup did not print one JSON object (exit ${code}): ${stdout}\n${stderr}`); }
  assert.equal(code, result.ok ? 0 : 1, 'the exit code follows ok');
  for (const key of ['changed', 'verified', 'notVerified', 'instructions', 'next'] as const) assert(Array.isArray(result[key]), `${key} is always a list`);
  return { code, result };
}

const calls = (target: Home): string[][] => fs.existsSync(target.log)
  ? fs.readFileSync(target.log, 'utf8').trim().split('\n').map(line => JSON.parse(line) as string[]) : [];

interface Hook { type: string; command: string; timeout?: number; asyncRewake?: boolean }
const hooksOf = (target: Home): Record<string, Array<{ hooks: Hook[] }>> =>
  (JSON.parse(fs.readFileSync(target.settings, 'utf8')) as { hooks: Record<string, Array<{ hooks: Hook[] }>> }).hooks;

function assertInstalled(target: Home, entry: string): void {
  const configFile = path.join(target.configRoot, 'claude', 'autojoin.json');
  assert.deepEqual(JSON.parse(fs.readFileSync(configFile, 'utf8')), { version: 1, mode: 'claude-autojoin',
    stateDirectory: path.join(target.configRoot, 'claude', 'sessions'), auditLog: path.join(target.configRoot, 'claude', 'network.jsonl'),
    enrollmentArgv: [process.execPath, entry, '--enroll-codex', '--config', path.join(target.configRoot, 'operator', 'operator.json')],
    peerPolicy: 'node-namespace' });
  const registered = (JSON.parse(fs.readFileSync(target.global, 'utf8')) as { mcpServers: Record<string, unknown> }).mcpServers.nexus_scode;
  assert.deepEqual(registered, { type: 'stdio', command: process.execPath, args: [entry, '--claude', '--config', configFile], env: {} });
  const bridge: Array<{ event: string; hook: Hook }> = [];
  for (const [event, groups] of Object.entries(hooksOf(target))) {
    for (const group of groups) for (const hook of group.hooks) if (/--claude-(watch|deliver)/.test(hook.command)) bridge.push({ event, hook });
  }
  assert.deepEqual(bridge.map(item => item.event).sort(), ['CwdChanged', 'FileChanged', 'SessionStart'], 'exactly the three bridge hooks');
  for (const { event, hook } of bridge) {
    const flag = event === 'FileChanged' ? '--claude-deliver' : '--claude-watch';
    assert.deepEqual(splitShellWords(hook.command), [process.execPath, entry, flag, '--config', configFile],
      'the written command line splits back into the exact argv, blanks included');
    assert.equal(hook.type, 'command');
    assert.equal(hook.asyncRewake, event === 'FileChanged' ? true : undefined, 'only the delivery hook wakes an idle session');
    assert.equal(hook.timeout, event === 'FileChanged' ? 60 : 30);
  }
}

async function main(): Promise<void> {
  assert.deepEqual(splitShellWords(`/a/node '/b c/server.cjs' --claude-watch --config "/d e/x.json"`),
    ['/a/node', '/b c/server.cjs', '--claude-watch', '--config', '/d e/x.json']);
  assert.deepEqual(splitShellWords(`'it'\\''s' ""  tail`), ["it's", '', 'tail']);

  // Node reports a started script by its real path, which is what setup registers.
  const base = fs.realpathSync.native(fs.mkdtempSync(path.join(os.tmpdir(), 'claude-setup-')));
  try {
    // A machine without a Nexus node: told what is missing, and nothing is created.
    const bare = home(base, 'bare', false);
    const noNode = await setup(bare, ['--issuer', 'https://founder.example:2126', '--caller-argv', JSON.stringify(['scode', 'a2a', 'setup', 'claude'])]);
    assert.equal(noNode.result.state, 'needs_input');
    assert.equal(noNode.result.changedAnything, false);
    assert.match(noNode.result.instructions.join(' '), /Join this machine to the Nexus cluster first/);
    assert.deepEqual(noNode.result.next[0].argv, ['scode', 'a2a', 'setup', 'claude', '--node-data-dir', '/ABSOLUTE/NODE/DATA'],
      'the next command repeats what the caller typed, plus the flag to fill');
    assert(!fs.existsSync(bare.configRoot) && !fs.existsSync(bare.global) && !fs.existsSync(bare.settings) && !calls(bare).length);

    // A new machine with a node but no grant yet: the issuer is asked for once.
    const fresh = home(base, 'fresh');
    const noIssuer = await setup(fresh);
    assert.equal(noIssuer.result.state, 'needs_input');
    assert.deepEqual(noIssuer.result.next[0].argv.slice(-2), ['--issuer', 'https://HOST:2126']);
    assert.deepEqual(noIssuer.result.next[0].argv.slice(0, 3), [process.execPath, server, '--setup-claude']);
    assert(!fs.existsSync(fresh.configRoot) && !fs.existsSync(fresh.global) && !calls(fresh).length, 'a missing input writes nothing');

    // The whole setup in one command.
    const first = await setup(fresh, ['--issuer', 'https://founder.example:2126']);
    assert.equal(first.result.state, 'installed', JSON.stringify(first.result));
    assert.equal(first.result.frontend, 'claude');
    assert.equal(first.result.changed.length, 5, 'grant, operator record, bridge config, MCP entry, hooks');
    assert.match(first.result.instructions[0], /Start a new Claude Code session/);
    assert.match(first.result.notVerified.join(' '), /No Claude Code session was started/);
    assertInstalled(fresh, server);
    const grant = JSON.parse(fs.readFileSync(path.join(fresh.configRoot, 'operator', 'node-grant.json'), 'utf8')) as Record<string, unknown>;
    assert.equal(grant.endpoint, 'https://founder.example:2126');
    assert.equal(grant.nodeDataDirectory, path.join(fresh.home, 'node-data'), 'the node data folder comes from the node record');
    const operator = JSON.parse(fs.readFileSync(path.join(fresh.configRoot, 'operator', 'operator.json'), 'utf8')) as { nexusArgv: string[]; scode: { scodeArgv: string[] } };
    assert.deepEqual(operator.nexusArgv, [...enrollArgv, '--grant', path.join(fresh.configRoot, 'operator', 'node-grant.json')]);
    assert.deepEqual(operator.scode.scodeArgv, [process.execPath, '--allowedTools', 'agent_list', '--allowedTools', 'send', 'acp']);
    assert.deepEqual(calls(fresh).map(call => call.slice(0, 2)), [['mcp', 'add-json']], 'one registration, and never `mcp get`');
    if (posix) assert.equal(fs.statSync(fresh.settings).mode & 0o777, 0o600);
    const journal = fs.readFileSync(first.result.journal!, 'utf8').trim().split('\n').map(line => JSON.parse(line) as { step: string });
    assert.deepEqual(journal.map(line => line.step), ['grant', 'operator', 'config', 'claude-cli', 'hooks', 'done']);

    // Running it again is the check: nothing changes, nothing is called.
    const before = [fresh.global, fresh.settings, path.join(fresh.configRoot, 'claude', 'autojoin.json')].map(digest);
    const again = await setup(fresh);
    assert.equal(again.result.state, 'already_installed');
    assert.deepEqual(again.result.changed, []);
    assert.deepEqual([fresh.global, fresh.settings, path.join(fresh.configRoot, 'claude', 'autojoin.json')].map(digest), before);
    assert.equal(calls(fresh).length, 1);

    // A different issuer for the same config root is refused, not rewritten.
    const otherIssuer = await setup(fresh, ['--issuer', 'https://elsewhere.example:2126']);
    assert.equal(otherIssuer.result.state, 'refused');
    assert.equal(otherIssuer.result.changedAnything, false);
    assert.deepEqual([fresh.global, fresh.settings].map(digest), before.slice(0, 2));

    // The same command from a newer SDK moves its own install and keeps the identities folder.
    const newer = path.join(base, 'newer sdk', 'dist');
    fs.mkdirSync(newer, { recursive: true });
    for (const name of ['server.cjs', 'index.cjs']) fs.copyFileSync(path.join(root, 'dist', name), path.join(newer, name));
    const newerServer = path.join(newer, 'server.cjs');
    const moved = await setup(fresh, [], { entry: newerServer });
    assert.equal(moved.result.state, 'moved_to_this_sdk', JSON.stringify(moved.result));
    assert.match(moved.result.changed.join('\n'), /moved the "nexus_scode" MCP entry[^\n]*\nmoved the three A2A hooks in /, 'a move is not reported as an addition');
    assertInstalled(fresh, newerServer);
    assert.deepEqual(calls(fresh).slice(1).map(call => call.slice(0, 2)), [['mcp', 'remove'], ['mcp', 'add-json']]);
    assert.equal((await setup(fresh, [], { entry: newerServer })).result.state, 'already_installed');

    // The user's own settings and hooks are kept, with their indentation and mode.
    const lived = home(base, 'lived-in');
    const own = { theme: 'dark', env: { A: '1' }, hooks: {
      SessionStart: [{ hooks: [{ type: 'command', command: 'echo started' }] }],
      PreToolUse: [{ matcher: 'Bash', hooks: [{ type: 'command', command: 'echo before' }] }] } };
    const original = JSON.stringify(own, null, 4);
    fs.writeFileSync(lived.settings, original, { mode: 0o640 });
    fs.writeFileSync(lived.global, JSON.stringify({ numStartups: 7, mcpServers: { other: { type: 'stdio', command: '/bin/other', args: [] } } }));
    const merged = await setup(lived, ['--issuer', 'https://founder.example:2126']);
    assert.equal(merged.result.state, 'installed', JSON.stringify(merged.result));
    assertInstalled(lived, server);
    const text = fs.readFileSync(lived.settings, 'utf8');
    const kept = JSON.parse(text) as typeof own;
    assert.equal(kept.theme, 'dark');
    assert.deepEqual(kept.env, { A: '1' });
    assert.deepEqual(kept.hooks.PreToolUse, own.hooks.PreToolUse);
    assert.deepEqual(kept.hooks.SessionStart[0], own.hooks.SessionStart[0], 'the user\'s own hook stays first');
    assert.match(text, /^\{\n {4}"theme"/, 'four-space indentation kept');
    assert(!text.endsWith('\n'), 'no trailing newline was added to a file that had none');
    if (posix) assert.equal(fs.statSync(lived.settings).mode & 0o777, 0o640);
    const backups = fs.readdirSync(path.join(lived.configRoot, 'claude', 'backups'));
    assert.equal(backups.length, 1);
    assert.equal(fs.readFileSync(path.join(lived.configRoot, 'claude', 'backups', backups[0]), 'utf8'), original);
    const globalAfter = JSON.parse(fs.readFileSync(lived.global, 'utf8')) as { numStartups: number; mcpServers: Record<string, unknown> };
    assert.equal(globalAfter.numStartups, 7);
    assert(globalAfter.mcpServers.other, 'other MCP servers are untouched');

    // An attachment this command did not make is refused with the way out, and nothing is written.
    for (const kind of ['entry', 'hook'] as const) {
      const foreign = home(base, 'foreign-' + kind);
      if (kind === 'entry') {
        fs.writeFileSync(foreign.global, JSON.stringify({ mcpServers: { nexus_scode: { type: 'stdio', command: '/usr/bin/node',
          args: ['/old/server.cjs', '--claude', '--config', '/hand/made/autojoin.json'], env: {} } } }));
      } else {
        fs.writeFileSync(foreign.settings, JSON.stringify({ hooks: { FileChanged: [{ hooks: [{ type: 'command', asyncRewake: true,
          command: '/usr/bin/node /old/server.cjs --claude-deliver --config /hand/made/autojoin.json' }] }] } }));
      }
      const file = kind === 'entry' ? foreign.global : foreign.settings;
      const bytes = digest(file);
      const refused = await setup(foreign, ['--issuer', 'https://founder.example:2126']);
      assert.equal(refused.result.state, 'refused', JSON.stringify(refused.result));
      assert.equal(refused.result.changedAnything, false);
      assert.match(refused.result.instructions.join(' '), /delete every hook entry whose command contains --claude-watch or --claude-deliver/);
      assert.deepEqual(refused.result.next[0].argv, [...claudeArgv, 'mcp', 'remove', 'nexus_scode', '-s', 'user']);
      assert.equal(digest(file), bytes);
      assert(!fs.existsSync(foreign.configRoot) && !calls(foreign).length, 'refused before the grant, the config or any claude call');
    }

    // Failures say what happened and whether anything was changed.
    const noTool = home(base, 'enroll-fails');
    const failed = await setup(noTool, ['--issuer', 'https://founder.example:2126'], { env: { NEXUS_FIXTURE_FAIL: '1' } });
    assert.equal(failed.result.state, 'failed');
    assert.match(failed.result.error!, /node certificate names another Node/);
    assert.equal(failed.result.changedAnything, false);
    assert(!fs.existsSync(noTool.global) && !fs.existsSync(noTool.settings));

    const silent = home(base, 'claude-ignores-add');
    const unregistered = await setup(silent, ['--issuer', 'https://founder.example:2126'], { env: { CLAUDE_FIXTURE_DROP_ADD: '1' } });
    assert.equal(unregistered.result.state, 'failed', 'an exit code of 0 is not taken as proof: the entry is read back');
    assert.match(unregistered.result.error!, /read back/);
    assert.equal(unregistered.result.changedAnything, true);
    assert(!fs.existsSync(silent.settings), 'hooks are not installed for an entry that is not there');
    // Running the same command again finishes the job.
    const resumed = await setup(silent);
    assert.equal(resumed.result.state, 'installed', JSON.stringify(resumed.result));
    assertInstalled(silent, server);

    // Without the test seams: no `claude` on PATH is a clear request, not a crash.
    const noClaude = home(base, 'no-claude');
    const missing = await setup(noClaude, ['--issuer', 'https://founder.example:2126'], { seams: false, env: { PATH: noClaude.home, Path: noClaude.home } });
    assert.equal(missing.result.state, 'needs_input');
    assert.deepEqual(missing.result.next[0].argv.slice(-2), ['--claude-command', '/ABSOLUTE/PATH/claude']);
  } finally {
    fs.rmSync(base, { recursive: true, force: true });
  }
  console.log(JSON.stringify({ claudeSetupIntegration: true }));
}

main().catch(error => { console.error(error); process.exitCode = 1; });
