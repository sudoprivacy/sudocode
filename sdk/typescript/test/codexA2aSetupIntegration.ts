import assert from 'node:assert/strict';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { WebSocketServer } from 'ws';

const root = path.resolve(__dirname, '../..');
const server = path.join(root, 'dist/server.cjs');
const { splitShellWords } = require(path.join(root, 'dist/index.cjs')) as typeof import('../src/index');
const codexArgv = [process.execPath, path.join(root, 'test/fixtures/codex-a2a-setup-cli.mjs')];
const enrollArgv = [process.execPath, path.join(root, 'test/fixtures/nexus-enroll-initialize.mjs')];
const TOOLS = ['scode_agent_list', 'scode_inbox', 'scode_send', 'scode_status'];

interface Result {
  ok: boolean; frontend: string; state: string; changed: string[]; verified: string[]; notVerified: string[];
  instructions: string[]; next: Array<{ why: string; argv: string[] }>; changedAnything?: boolean; journal?: string; error?: string;
}

interface Home { home: string; codexHome: string; configRoot: string; bin: string; port: number; env: Record<string, string>; toml: string; config: string }

async function freePort(): Promise<number> {
  const listener = net.createServer();
  listener.listen(0, '127.0.0.1');
  await once(listener, 'listening');
  const { port } = listener.address() as net.AddressInfo;
  listener.close();
  await once(listener, 'close');
  return port;
}

async function main(): Promise<void> {
  // Node reports a started script by its real path, which is what setup registers.
  const base = fs.realpathSync.native(fs.mkdtempSync(path.join(os.tmpdir(), 'codex-a2a-setup-')));
  // Stands in for Codex's background server behind `codex app-server proxy`.
  const native = new WebSocketServer({ host: '127.0.0.1', port: 0 });
  await once(native, 'listening');
  const nativePort = (native.address() as net.AddressInfo).port;
  // Every listener a case opens, so a failed assertion cannot leave the process waiting on one.
  const listeners: net.Server[] = [];
  const occupy = async (port: number): Promise<net.Server> => {
    const listener = net.createServer(socket => socket.destroy());
    listeners.push(listener);
    listener.listen(port, '127.0.0.1');
    await once(listener, 'listening');
    return listener;
  };
  const release = async (listener: net.Server): Promise<void> => { listener.close(); await once(listener, 'close'); };
  native.on('connection', socket => socket.on('message', bytes => {
    const call = JSON.parse(bytes.toString()) as { id?: number; method: string };
    if (call.method === 'initialize') socket.send(JSON.stringify({ id: call.id, result: { userAgent: 'codex-a2a-setup-fixture' } }));
    else assert.equal(call.method, 'initialized', 'setup must not read threads or submit input');
  }));

  /** A private home. PATH holds only `bin`, so no real codex or launcher on this machine is found. */
  const home = async (name: string, codexHomeExists = true): Promise<Home> => {
    const dir = path.join(base, name);
    const codexHome = path.join(dir, 'codex home');
    const bin = path.join(dir, 'bin');
    fs.mkdirSync(bin, { recursive: true });
    if (codexHomeExists) fs.mkdirSync(codexHome, { recursive: true });
    fs.mkdirSync(path.join(dir, 'node-data', 'agents'), { recursive: true });
    fs.mkdirSync(path.join(dir, '.nexus', 'run'), { recursive: true });
    fs.writeFileSync(path.join(dir, '.nexus', 'run', 'node.json'), JSON.stringify({ version: 1, agents_dir: path.join(dir, 'node-data', 'agents') }));
    const env: Record<string, string> = { HOME: dir, USERPROFILE: dir, CODEX_HOME: codexHome, PATH: bin, SCODE_SETUP_PROXY_PORT: String(nativePort) };
    for (const key of ['SystemRoot', 'TEMP', 'TMP', 'TMPDIR']) if (process.env[key]) env[key] = process.env[key]!;
    const configRoot = path.join(dir, 'a2a root');
    return { home: dir, codexHome, configRoot, bin, port: await freePort(), env, toml: path.join(codexHome, 'config.toml'),
      config: path.join(configRoot, 'codex', 'runtime.json') };
  };
  const setup = async (target: Home, extra: string[] = [], options: { entry?: string; env?: Record<string, string>; port?: boolean } = {}): Promise<Result> => {
    const argv = [options.entry ?? server, '--setup-codex', '--config-root', target.configRoot, '--codex-argv', JSON.stringify(codexArgv),
      '--nexus-enroll-argv', JSON.stringify(enrollArgv), '--scode', process.execPath,
      ...(options.port === false ? [] : ['--port', String(target.port)]), ...extra];
    const child = spawn(process.execPath, argv, { env: { ...target.env, ...options.env }, shell: false, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] });
    let stdout = '', stderr = '';
    child.stdout.on('data', bytes => stdout += String(bytes));
    child.stderr.on('data', bytes => stderr += String(bytes));
    const [code] = await once(child, 'close');
    let result: Result;
    try { result = JSON.parse(stdout) as Result; } catch { throw new Error(`setup did not print one JSON object (exit ${code}): ${stdout}\n${stderr}`); }
    assert.equal(code, result.ok ? 0 : 1, 'the exit code follows ok');
    assert.equal(result.frontend, 'codex');
    return result;
  };
  const toml = (target: Home): string => fs.existsSync(target.toml) ? fs.readFileSync(target.toml, 'utf8') : '';
  const calls = (target: Home): string[] => fs.existsSync(path.join(target.codexHome, 'codex-cli.log'))
    ? fs.readFileSync(path.join(target.codexHome, 'codex-cli.log'), 'utf8').trim().split('\n').map(line => (JSON.parse(line) as string[]).slice(0, 3).join(' ')) : [];
  const approved = (target: Home): string[] => [...toml(target).matchAll(/^\[mcp_servers\.nexus_scode\.tools\.(\w+)\]\napproval_mode = "approve"$/gm)].map(match => match[1]).sort();
  /** The entry, and the Codex home it carries for the bridge (none for the default home). */
  const assertRegistered = (target: Home, entry: string, carriesHome = true): void => {
    const args = JSON.stringify([entry, '--codex', '--config', target.config]);
    assert(toml(target).includes(`[mcp_servers.nexus_scode]\ncommand = ${JSON.stringify(process.execPath)}\nargs = ${args}\n`), toml(target));
    const carried = `[mcp_servers.nexus_scode.env]\nCODEX_HOME = ${JSON.stringify(target.codexHome)}\n`;
    assert.equal(toml(target).includes(carried), carriesHome, toml(target));
    if (!carriesHome) assert(!toml(target).includes('[mcp_servers.nexus_scode.env]'), toml(target));
  };

  try {
    if (process.platform === 'win32') {
      const windows = await setup(await home('windows'), ['--issuer', 'https://founder.example:2126']);
      assert.equal(windows.state, 'refused');
      assert.match(windows.instructions.join(' '), /--install-codex/);
      console.log(JSON.stringify({ codexA2aSetupIntegration: 'windows refusal only' }));
      return;
    }

    // Codex was never started on this machine: nothing to attach to yet.
    const unused = await home('unused', false);
    const never = await setup(unused, ['--issuer', 'https://founder.example:2126']);
    assert.equal(never.state, 'needs_input');
    assert.match(never.instructions.join(' '), /Start Codex once and sign in/);
    assert(!fs.existsSync(unused.configRoot));

    // The issuer is asked for once, and a missing input writes nothing.
    const fresh = await home('fresh');
    const noIssuer = await setup(fresh);
    assert.equal(noIssuer.state, 'needs_input');
    assert.deepEqual(noIssuer.next[0].argv.slice(-2), ['--issuer', 'https://HOST:2126']);
    assert(!fs.existsSync(fresh.configRoot) && toml(fresh) === '' && !calls(fresh).length);

    // The whole setup in one command.
    const first = await setup(fresh, ['--issuer', 'https://founder.example:2126']);
    assert.equal(first.state, 'installed', JSON.stringify(first));
    assert.equal(first.changed.length, 5, 'grant, operator record, bridge config, background server, MCP entry');
    assertRegistered(fresh, server);
    const runtime = JSON.parse(fs.readFileSync(fresh.config, 'utf8')) as Record<string, unknown> & { endpoint: { port: number; token: string } };
    assert.deepEqual({ ...runtime, endpoint: undefined }, { version: 1, mode: 'codex-autojoin', native: { codexArgv },
      stateDirectory: path.join(fresh.configRoot, 'codex', 'codex-agents'), auditLog: path.join(fresh.configRoot, 'codex', 'codex-network.jsonl'),
      peers: [], peerPolicy: 'node-namespace', peerInputVersion: 3,
      enrollmentArgv: [process.execPath, server, '--enroll-codex', '--config', path.join(fresh.configRoot, 'operator', 'operator.json')], endpoint: undefined });
    assert.equal(runtime.endpoint.port, fresh.port);
    assert.match(runtime.endpoint.token, /^[0-9a-f]{64}$/);
    assert.deepEqual(calls(fresh), ['app-server daemon start', 'app-server proxy', 'mcp add nexus_scode'],
      'the background server is started, asked once through the proxy, then the entry is registered');
    assert.deepEqual(approved(fresh), [], 'no permission is granted unless asked for');
    assert.match(first.instructions.join(' '), /Codex asks before it calls scode_inbox, scode_send, scode_agent_list, scode_status/);
    assert.equal(first.next[0].argv.at(-1), '--approve-tools');
    assert.match(first.instructions.join(' '), /approvals_reviewer=user/);
    assert.match(first.notVerified.join(' '), /No Codex session was started/);
    assert(first.verified.some(line => line.includes(path.join(fresh.codexHome, 'app-server-control'))), 'the server the bridge will reach is named');

    // Running it again is the check.
    const before = toml(fresh);
    const again = await setup(fresh);
    assert.equal(again.state, 'already_installed');
    assert.deepEqual(again.changed, []);
    assert.equal(toml(fresh), before);
    assert.deepEqual(calls(fresh).slice(3), ['app-server proxy'], 'only the read-only server check ran');

    // Tool approvals are written only on request, appended to whatever the user has.
    fs.appendFileSync(fresh.toml, '\n[mcp_servers.other]\ncommand = "/bin/other"\n');
    const allowed = await setup(fresh, ['--approve-tools']);
    assert.equal(allowed.state, 'installed', JSON.stringify(allowed));
    assert.equal(allowed.changed.length, 1);
    assert.match(allowed.changed[0], /allowed Codex to call scode_agent_list, scode_inbox, scode_send, scode_status without asking/);
    assert.deepEqual(approved(fresh), TOOLS);
    assert(toml(fresh).includes('[mcp_servers.other]\ncommand = "/bin/other"\n'), 'the user\'s other servers stay');
    assertRegistered(fresh, server);
    assert.deepEqual(allowed.next, [], 'nothing is left to allow');
    assert.equal((await setup(fresh, ['--approve-tools'])).state, 'already_installed');

    // A newer package cannot take over while the old bridge serves open sessions.
    const newer = path.join(base, 'newer sdk', 'dist');
    fs.mkdirSync(newer, { recursive: true });
    for (const name of ['server.cjs', 'index.cjs']) fs.copyFileSync(path.join(root, 'dist', name), path.join(newer, name));
    const newerServer = path.join(newer, 'server.cjs');
    const owner = await occupy(fresh.port);
    const busy = await setup(fresh, [], { entry: newerServer, port: false });
    assert.equal(busy.state, 'refused');
    assert.equal(busy.changedAnything, false);
    assert.match(busy.instructions.join(' '), /Close every Codex session\. Wait about two minutes/);
    assertRegistered(fresh, server);
    await release(owner);

    // With the sessions closed it moves, and keeps the port, the token and the approvals `codex mcp add` drops.
    const moved = await setup(fresh, [], { entry: newerServer, port: false });
    assert.equal(moved.state, 'moved_to_this_sdk', JSON.stringify(moved));
    assertRegistered(fresh, newerServer);
    assert.deepEqual(approved(fresh), TOOLS, 'approvals survive the move');
    assert(!moved.changed.some(line => line.includes('allowed Codex')), 'restoring what the user had is not reported as a new permission');
    const after = JSON.parse(fs.readFileSync(fresh.config, 'utf8')) as { endpoint: unknown; enrollmentArgv: string[] };
    assert.deepEqual(after.endpoint, runtime.endpoint);
    assert.equal(after.enrollmentArgv[1], newerServer);
    assert.equal((await setup(fresh, [], { entry: newerServer, port: false })).state, 'already_installed');

    // A launcher is written only where the caller names it, and moves with the package.
    const launching = await home('launcher');
    const real = path.join(launching.home, 'real-codex.js');
    fs.writeFileSync(real, '// the real program\n');
    fs.symlinkSync(real, path.join(launching.bin, 'codex'));
    const launched = await setup(launching, ['--issuer', 'https://founder.example:2126', '--command-entry', path.join(launching.bin, 'codex')]);
    assert.equal(launched.state, 'installed', JSON.stringify(launched));
    const script = fs.readFileSync(path.join(launching.bin, 'codex'), 'utf8');
    assert(!fs.lstatSync(path.join(launching.bin, 'codex')).isSymbolicLink(), 'the link itself is replaced');
    assert.equal(fs.readFileSync(real, 'utf8'), '// the real program\n', 'what the link pointed to is untouched');
    assert.deepEqual(splitShellWords(script.split('\n')[1].slice(5)), [process.execPath, server, '--launch-codex', '--config', launching.config, '--', '$@']);
    assert.equal(fs.statSync(path.join(launching.bin, 'codex')).mode & 0o777, 0o755);
    assert(!launched.instructions.join(' ').includes('approvals_reviewer'), 'no launcher advice once one is installed');
    const journal = fs.readFileSync(launched.journal!, 'utf8');
    assert(journal.includes('"previous":"symlink to ' + real), 'what was replaced is recorded');
    const relaunched = await setup(launching, [], { entry: newerServer, port: false });
    assert.equal(relaunched.state, 'moved_to_this_sdk', JSON.stringify(relaunched));
    assert.equal(splitShellWords(fs.readFileSync(path.join(launching.bin, 'codex'), 'utf8').split('\n')[1].slice(5))[1], newerServer,
      'a launcher found on PATH moves without being named again');

    // The default Codex home needs nothing carried: a bridge started by Codex reads it anyway.
    const plain = await home('default-home');
    plain.codexHome = path.join(plain.home, '.codex');
    plain.toml = path.join(plain.codexHome, 'config.toml');
    fs.mkdirSync(plain.codexHome);
    delete plain.env.CODEX_HOME;
    const defaulted = await setup(plain, ['--issuer', 'https://founder.example:2126']);
    assert.equal(defaulted.state, 'installed', JSON.stringify(defaulted));
    assertRegistered(plain, server, false);
    assert.equal((await setup(plain)).state, 'already_installed');

    // Codex selected in a way a bridge started by Codex cannot follow: the bridge would read
    // another server, and enroll agents for that server's sessions. Refused before registering.
    const elsewhere = await home('unreachable-home');
    const otherHome = path.join(elsewhere.home, 'other codex home');
    fs.mkdirSync(otherHome);
    elsewhere.toml = path.join(otherHome, 'config.toml');
    const stray = await setup(elsewhere, ['--issuer', 'https://founder.example:2126'], { env: { CODEX_FIXTURE_OTHER_HOME: otherHome } });
    assert.equal(stray.state, 'refused', JSON.stringify(stray));
    assert.match(stray.error!, /would not reach the Codex server this command set up/);
    assert.match(stray.instructions.join(' '), /Nothing was registered with Codex/);
    assert.equal(toml(elsewhere), '', 'no entry in the home the command used');
    assert(!fs.existsSync(path.join(elsewhere.codexHome, 'config.toml')), 'and none in the home the bridge would have read');

    // A real program is never replaced by a launcher.
    const program = await home('program');
    fs.writeFileSync(path.join(program.bin, 'codex'), Buffer.from([0x7f, 0x45, 0x4c, 0x46, 0, 0, 0]), { mode: 0o755 });
    const kept = await setup(program, ['--issuer', 'https://founder.example:2126', '--command-entry', path.join(program.bin, 'codex')]);
    assert.equal(kept.state, 'needs_input');
    assert(!fs.existsSync(program.configRoot));

    // An attachment this command did not make is refused with the way out.
    for (const kind of ['entry', 'launcher'] as const) {
      const foreign = await home('foreign-' + kind);
      if (kind === 'entry') {
        fs.writeFileSync(foreign.toml, '[mcp_servers.nexus_scode]\ncommand = "/usr/bin/node"\nargs = ["/old/server.cjs","--codex","--config","/hand/made/runtime.json"]\n');
      } else {
        fs.writeFileSync(path.join(foreign.bin, 'codex'), "#!/bin/sh\nexec /usr/bin/node /old/server.cjs --launch-codex --config /hand/made/runtime.json -- \"$@\"\n", { mode: 0o755 });
      }
      const text = toml(foreign);
      const refused = await setup(foreign, ['--issuer', 'https://founder.example:2126']);
      assert.equal(refused.state, 'refused', JSON.stringify(refused));
      assert.equal(refused.changedAnything, false);
      assert.deepEqual(refused.next[0].argv, [...codexArgv, 'mcp', 'remove', 'nexus_scode']);
      assert.match(refused.instructions.join(' '), /Close every Codex session and wait about two minutes/);
      assert.equal(toml(foreign), text);
      assert(!fs.existsSync(foreign.configRoot) && !calls(foreign).length);
    }

    // The default port taken by another program on a new machine: asked for another one.
    const crowded = await home('crowded');
    const other = await occupy(crowded.port);
    const taken = await setup(crowded, ['--issuer', 'https://founder.example:2126']);
    await release(other);
    assert.equal(taken.state, 'needs_input');
    assert.deepEqual(taken.next[0].argv.slice(-2), ['--port', String(crowded.port + 1)]);
    assert(!fs.existsSync(crowded.configRoot));

    // Codex's background server does not come up: stopped before anything is registered.
    const broken = await home('daemon-broken');
    const failed = await setup(broken, ['--issuer', 'https://founder.example:2126'], { env: { CODEX_FIXTURE_DAEMON_BROKEN: '1' } });
    assert.equal(failed.state, 'failed');
    assert.match(failed.error!, /background server did not start/);
    assert.equal(failed.changedAnything, true, 'the grant and config were already written, and say so');
    assert.equal(toml(broken), '', 'nothing was registered with Codex');
    // The same command finishes the job once the cause is gone.
    const resumed = await setup(broken);
    assert.equal(resumed.state, 'installed', JSON.stringify(resumed));
    assertRegistered(broken, server);
  } finally {
    for (const listener of listeners) if (listener.listening) listener.close();
    for (const client of native.clients) client.terminate();
    native.close();
    fs.rmSync(base, { recursive: true, force: true });
  }
  console.log(JSON.stringify({ codexA2aSetupIntegration: true }));
}

main().catch(error => { console.error(error); process.exitCode = 1; });
