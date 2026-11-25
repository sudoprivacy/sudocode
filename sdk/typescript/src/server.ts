#!/usr/bin/env node
import { createHash } from 'node:crypto';
import * as fs from 'node:fs';
import * as path from 'node:path';
import { parseArgs } from 'node:util';
import { runScodeMcp } from './mcp';
import { runCodexMcp, runCodexMcpOwner, prepareCodexMcp, prepareExistingCodexBinding, prepareWindowsCodexOwner, launchCodexFromConfig, recoverCompletedCodexBinding } from './codexMcp';
import { installCommandShutdownSignals } from './lifecycle';
import { runCodexEnrollment, prepareCodexEnrollment, refreshCodexProfiles } from './codexEnrollment';
import { installCodexMcp, checkCodexNative } from './codexSetup';
import { captureReceiverOwner } from './receiverOwner';
import { claudeDeliver, claudeWatch, runClaudeMcp } from './claudeAutojoin';
import { setupClaude } from './claudeSetup';
import { setupCodex } from './codexA2aSetup';

interface Manifest {
  sourceCommit: string;
  files: Record<string, { bytes: number; sha256: string }>;
}

function manifest(): Manifest {
  return JSON.parse(fs.readFileSync(path.join(__dirname, '../manifest.json'), 'utf8')) as Manifest;
}

function verify(): void {
  const description = manifest();
  if (!/^[0-9a-f]{40}$/.test(description.sourceCommit) || !description.files) {
    throw new Error('Invalid package manifest');
  }
  for (const [name, expected] of Object.entries(description.files)) {
    if (!/^[a-zA-Z0-9._/-]+$/.test(name) || name.split('/').some(part => !part || part === '.' || part === '..')) {
      throw new Error('Invalid package filename');
    }
    const file = path.join(__dirname, '..', name);
    if (!fs.lstatSync(file).isFile()) throw new Error(`Package entry is not a regular file: ${name}`);
    const bytes = fs.readFileSync(file);
    if (bytes.length !== expected.bytes || createHash('sha256').update(bytes).digest('hex') !== expected.sha256) {
      throw new Error(`Package integrity mismatch: ${name}`);
    }
  }
  console.log(JSON.stringify({ verified: true, sourceCommit: description.sourceCommit }));
}

async function main(): Promise<void> {
  const { values, positionals } = parseArgs({ allowPositionals:true, options: {
    config: { type: 'string' }, help: { type: 'boolean' }, version: { type: 'boolean' },
    'capture-receiver-owner': { type: 'string' },
    verify: { type: 'boolean' }, init: { type: 'boolean' }, scode: { type: 'string' },
    workspace: { type: 'string' }, 'session-id': { type: 'string' },
    codex: { type: 'boolean' }, 'codex-owner': { type: 'boolean' },
    'registration-id': { type: 'string' },
    'prepare-codex-owner': { type: 'boolean' },
    'launch-codex': {type:'boolean'},
    'install-codex': {type:'boolean'}, 'command-entry': {type:'string'},
    'check-codex-native': {type:'boolean'},
    'init-codex': {type:'boolean'}, 'native-codex': {type:'string'}, 'native-argv': {type:'string'},
    'native-env-file': {type:'string'},
    enrollment: {type:'string'}, peers: {type:'string'}, port: {type:'string'},
    'peer-policy': {type:'string'}, 'peer-input-version': {type:'string'},
    'bind-codex': {type:'boolean'}, binding: {type:'string'},
    'recover-codex-binding': {type:'string'},
    'enroll-codex': {type:'boolean'}, 'refresh-codex-profiles': {type:'boolean'},
    'claude-watch': {type:'boolean'}, 'claude-deliver': {type:'boolean'}, claude: {type:'boolean'},
    'init-codex-operator': {type:'boolean'}, 'nexus-operator': {type:'string'}, 'scode-argv': {type:'string'}, 'scode-env': {type:'string'},
    'setup-claude': {type:'boolean'}, issuer: {type:'string'}, 'node-data-dir': {type:'string'}, 'nexus-enroll': {type:'string'},
    'nexus-enroll-argv': {type:'string'}, 'config-root': {type:'string'}, 'claude-command': {type:'string'}, 'claude-argv': {type:'string'},
    'caller-argv': {type:'string'},
    'setup-codex': {type:'boolean'}, 'codex-command': {type:'string'}, 'codex-argv': {type:'string'}, 'approve-tools': {type:'boolean'},
  } });
  if (values['registration-id'] !== undefined && (!values.codex
      || Object.keys(values).some(key => !['config','codex','registration-id'].includes(key)))) {
    throw new Error('--registration-id applies only to the configured Codex MCP client');
  }
  if(positionals.length&&!values['launch-codex'])throw new Error('Native arguments require --launch-codex and --');
  if (values.help) {
    console.log('scode-acp-bridge --setup-claude [--issuer https://HOST:2126]  (attach Claude Code to the agent network: one command, safe to run again)\n'
      + '  Discovers the Nexus node, the enrollment tool, scode and claude; override with --node-data-dir, --nexus-enroll, --scode, --claude-command, --config-root.\n'
      + '  Prints one JSON object: ok, state, changed, verified, instructions (what to do next, in order) and next (exact commands).\n'
      + 'scode-acp-bridge --setup-codex [--issuer https://HOST:2126] [--approve-tools] [--command-entry PATH] [--port N]  (attach Codex: one command, safe to run again; not on Windows yet)\n'
      + '  Close every Codex session first when moving to a newer package. Same JSON result as --setup-claude.\n'
      + 'scode-acp-bridge --config /absolute/private/runtime.json\n'
      + 'scode-acp-bridge --codex --config FILE [--registration-id UUID]  (MCP stdio client)\n'
      + 'scode-acp-bridge --capture-receiver-owner PID  (read-only Windows identity; place once in private config receiverOwner)\n'
      + 'scode-acp-bridge --verify | --version\n'
      + 'scode-acp-bridge --init --config FILE --scode EXECUTABLE --workspace DIRECTORY --session-id EXISTING_ID\n'
      + 'scode-acp-bridge --init-codex --config FILE --native-codex EXECUTABLE --enrollment JSON_ARGV --peers JSON_NAMES [--port 23262]\n'
      + '  Use --native-argv JSON_ARGV instead of --native-codex to preserve an official wrapper command.\n'
      + '  --native-env-file PRIVATE_JSON pins the complete native environment, including CODEX_HOME.\n'
      + '  --peer-policy node-namespace explicitly accepts peer DATA from the verified Node namespace; default: allowlist.\n'
      + '  --peer-input-version 3 reads peer frames through scode_inbox and wakes an idle root (requires node-namespace); default: 2.\n'
      + 'scode-acp-bridge --bind-codex --config FILE --binding EXISTING_MAPPING_FILE  (prepare only, while bridge is stopped)\n'
      + 'scode-acp-bridge --recover-codex-binding THREAD_ID --config FILE  (settle a completed enrollment while the owner is stopped)\n'
      + 'scode-acp-bridge --codex --config FILE  (global Codex MCP entry)\n'
      + 'scode-acp-bridge --claude --config FILE  (Claude Code MCP entry: one Agent per session, no launch flag)\n'
      + 'scode-acp-bridge --claude-watch --config FILE  (Claude Code SessionStart and CwdChanged hook: watch the session\'s delivery spool)\n'
      + 'scode-acp-bridge --claude-deliver --config FILE  (Claude Code FileChanged asyncRewake hook: exit 2 with new deliveries)\n'
      + 'scode-acp-bridge --launch-codex --config FILE -- NATIVE_ARGS  (ordinary foreground launch; preserves configured user review)\n'
      + 'scode-acp-bridge --install-codex --config FILE --command-entry PUBLIC_CODEX  (one-time installation of a prepared authorized machine)\n'
      + 'scode-acp-bridge --check-codex-native --config FILE  (read-only receiver capability check on the existing native daemon)\n'
      + 'scode-acp-bridge --prepare-codex-owner --config FILE  (prepare Windows current-user task; does not register/start it)\n'
      + 'scode-acp-bridge --enroll-codex --config OWNER_GRANT  (operator adapter, public JSON stdin)\n'
      + 'scode-acp-bridge --refresh-codex-profiles --config OWNER_GRANT  (republish enrolled actors\' profiles under the current launch policy)\n'
      + 'scode-acp-bridge --init-codex-operator --config FILE --nexus-operator JSON_ARGV --scode-argv JSON_ARGV [--scode-env JSON_OBJECT]\n'
      + 'Uses the shared scode ACP/MCP implementation; no Hydra application, tmux or Nexus daemon is started.');
    return;
  }
  if (values.version) {
    console.log(JSON.stringify({ implementation: 'scode-acp-bridge', sourceCommit: manifest().sourceCommit }));
    return;
  }
  if (values.verify) { verify(); return; }
  if (values['capture-receiver-owner'] !== undefined) {
    if (Object.keys(values).some(key => key !== 'capture-receiver-owner')) throw new Error('Select one capture mode');
    console.log(JSON.stringify(await captureReceiverOwner(Number(values['capture-receiver-owner']))));
    return;
  }
  if (values['setup-claude']) {
    const allowed = ['setup-claude','issuer','node-data-dir','nexus-enroll','nexus-enroll-argv','scode','config-root','claude-command','claude-argv','caller-argv'];
    if (Object.entries(values).some(([key, value]) => value !== undefined && !allowed.includes(key))) throw new Error('--setup-claude takes only its own options; see --help');
    const list = (name: 'nexus-enroll-argv' | 'claude-argv' | 'caller-argv'): string[] | undefined =>
      values[name] === undefined ? undefined : JSON.parse(values[name]!) as string[];
    const result = await setupClaude({ entry: __filename, issuer: values.issuer, nodeDataDir: values['node-data-dir'],
      nexusEnroll: values['nexus-enroll'], nexusEnrollArgv: list('nexus-enroll-argv'), scode: values.scode,
      configRoot: values['config-root'], claudeCommand: values['claude-command'], claudeArgv: list('claude-argv'),
      // What the caller typed, so `next` can repeat it with one flag added.
      callerArgv: list('caller-argv') ?? [process.execPath, __filename, ...process.argv.slice(2)] });
    console.log(JSON.stringify(result, null, 2));
    if (!result.ok) process.exitCode = 1;
    return;
  }
  if (values['setup-codex']) {
    const allowed = ['setup-codex','issuer','node-data-dir','nexus-enroll','nexus-enroll-argv','scode','config-root','codex-command','codex-argv',
      'caller-argv','port','command-entry','approve-tools'];
    if (Object.entries(values).some(([key, value]) => value !== undefined && !allowed.includes(key))) throw new Error('--setup-codex takes only its own options; see --help');
    const list = (name: 'nexus-enroll-argv' | 'codex-argv' | 'caller-argv'): string[] | undefined =>
      values[name] === undefined ? undefined : JSON.parse(values[name]!) as string[];
    const result = await setupCodex({ entry: __filename, issuer: values.issuer, nodeDataDir: values['node-data-dir'],
      nexusEnroll: values['nexus-enroll'], nexusEnrollArgv: list('nexus-enroll-argv'), scode: values.scode,
      configRoot: values['config-root'], codexCommand: values['codex-command'], codexArgv: list('codex-argv'),
      port: values.port === undefined ? undefined : Number(values.port), commandEntry: values['command-entry'],
      approveTools: values['approve-tools'],
      callerArgv: list('caller-argv') ?? [process.execPath, __filename, ...process.argv.slice(2)] });
    console.log(JSON.stringify(result, null, 2));
    if (!result.ok) process.exitCode = 1;
    return;
  }
  if (!values.config || !path.isAbsolute(values.config)) throw new Error('--config must be an absolute path');
  if(values['check-codex-native']){
    if(Object.entries(values).some(([key,value])=>value!==undefined&&!['config','check-codex-native'].includes(key)))throw new Error('Native preflight takes only --config');
    console.log(JSON.stringify(await checkCodexNative(values.config,__filename)));return;
  }
  if(values['install-codex']){
    if(Object.entries(values).some(([key,value])=>value!==undefined&&!['config','install-codex','command-entry'].includes(key)))throw new Error('Select one installation mode');
    if(!values['command-entry'])throw new Error('--command-entry must select the ordinary Codex launcher');
    console.log(JSON.stringify(await installCodexMcp(values.config,__filename,values['command-entry'])));return;
  }
  if(values['command-entry']&&!values['init-codex'])throw new Error('--command-entry requires --install-codex or --init-codex');
  if(values['launch-codex']){
    if(Object.entries(values).some(([key,value])=>value!==undefined&&!['config','launch-codex'].includes(key)))throw new Error('Select one foreground launch mode');
    process.exitCode=await launchCodexFromConfig(values.config,positionals);return;
  }
  if(values['prepare-codex-owner']){
    if(Object.entries(values).some(([key,value])=>value!==undefined && !['config','prepare-codex-owner'].includes(key)))throw new Error('Select one owner preparation mode');
    console.log(JSON.stringify(prepareWindowsCodexOwner(values.config,__filename)));
    return;
  }
  if(values['init-codex-operator']){
    if(Object.entries(values).some(([key,value])=>value!==undefined && !['config','init-codex-operator','nexus-operator','scode-argv','scode-env'].includes(key)))throw new Error('Select one operator initializer mode');
    console.log(JSON.stringify(prepareCodexEnrollment(values.config,JSON.parse(values['nexus-operator']??'null'),
      {scodeArgv:JSON.parse(values['scode-argv']??'null'),...(values['scode-env']?{env:JSON.parse(values['scode-env'])}:{})},__filename)));
    return;
  }
  if(values['nexus-operator']||values['scode-argv']||values['scode-env'])throw new Error('Operator initializer fields require --init-codex-operator');
  if(values['refresh-codex-profiles']){
    if(Object.entries(values).some(([key,value])=>value!==undefined && !['config','refresh-codex-profiles'].includes(key)))throw new Error('--refresh-codex-profiles takes only --config');
    const refreshed=refreshCodexProfiles(values.config);
    for(const row of refreshed)console.log(JSON.stringify(row));
    if(refreshed.some(row=>row.outcome==='refused'))process.exitCode=1;
    return;
  }
  if(values['enroll-codex']){
    if(Object.entries(values).some(([key,value])=>value!==undefined && !['config','enroll-codex'].includes(key)))throw new Error('Select one operator enrollment mode');
    await runCodexEnrollment(values.config);return;
  }
  if(values['claude-watch']||values['claude-deliver']){
    const hook=values['claude-watch']?'claude-watch':'claude-deliver';
    if(Object.entries(values).some(([key,value])=>value!==undefined&&!['config',hook].includes(key)))throw new Error(`--${hook} takes only --config`);
    const input=await hookInput();
    if(hook==='claude-watch'){const output=claudeWatch(values.config,input);if(output)console.log(output);return;}
    // Exit 2 hands stderr to Claude: mid-turn, or by waking an idle session.
    const text=claudeDeliver(values.config,input);
    if(text){process.stderr.write(text);process.exitCode=2;}
    return;
  }
  if(values.claude){
    if(Object.entries(values).some(([key,value])=>value!==undefined&&!['config','claude'].includes(key)))throw new Error('--claude takes only --config');
    installCommandShutdownSignals();
    await runClaudeMcp(values.config);return;
  }
  if(values['recover-codex-binding'] !== undefined){
    if(Object.keys(values).some(key=>!['config','recover-codex-binding'].includes(key)))throw new Error('Select one binding recovery mode');
    console.log(JSON.stringify(await recoverCompletedCodexBinding(values.config,values['recover-codex-binding'])));
    return;
  }
  if(values['bind-codex']){
    if(values.init||values['init-codex']||values.codex||values['codex-owner']||values.scode||values.workspace
      ||values['session-id']||values['native-codex']||values['native-argv']||values['native-env-file']||values.enrollment||values.peers||values.port||values['peer-policy']||values['peer-input-version'])throw new Error('Select one preparation mode');
    if(!values.binding||!path.isAbsolute(values.binding))throw new Error('--binding must be an absolute private mapping file');
    console.log(JSON.stringify(await prepareExistingCodexBinding(values.config,JSON.parse(fs.readFileSync(values.binding,'utf8')))));
    return;
  }
  if(values.binding)throw new Error('--binding requires --bind-codex');
  if(values['init-codex']){
    if(values.init||values.codex||values['codex-owner']||values.scode||values.workspace||values['session-id'])throw new Error('Select one initializer mode');
    if(Boolean(values['native-codex'])===Boolean(values['native-argv']))throw new Error('Select exactly one native executable or argv');
    if(values['native-env-file']&&!path.isAbsolute(values['native-env-file']))throw new Error('--native-env-file must be an absolute private file');
    const nativeArgv=values['native-argv']?JSON.parse(values['native-argv']):[values['native-codex']!];
    console.log(JSON.stringify(prepareCodexMcp(values.config,values['native-env-file']
      ? {codexArgv:nativeArgv,env:JSON.parse(fs.readFileSync(values['native-env-file'],'utf8'))} : nativeArgv,JSON.parse(values.enrollment??'null'),
      JSON.parse(values.peers??'null'),__filename,values.port===undefined?undefined:Number(values.port),
      values['peer-policy'] as 'allowlist'|'node-namespace'|undefined,values['command-entry'],
      values['peer-input-version']===undefined?undefined:Number(values['peer-input-version']) as 2|3)));
    return;
  }
  if(values['native-codex']||values['native-argv']||values['native-env-file']||values.enrollment||values.peers||values.port||values['peer-policy']||values['peer-input-version'])throw new Error('Codex initializer fields require --init-codex');
  if (values.init) {
    const executable = values.scode;
    const workspace = values.workspace;
    const sessionId = values['session-id'];
    if (!executable || !path.isAbsolute(executable) || !fs.statSync(executable).isFile()
        || !workspace || !path.isAbsolute(workspace) || !fs.statSync(workspace).isDirectory()
        || !sessionId?.trim()) {
      throw new Error('init requires an existing absolute executable/workspace and an explicit existing session ID');
    }
    fs.mkdirSync(path.dirname(values.config), { recursive: true, mode: 0o700 });
    fs.writeFileSync(values.config, JSON.stringify({ version: 1, scodeArgv: [executable, 'acp'],
      workspace, acpSessionId: sessionId,
      auditLog: path.join(path.dirname(values.config), 'external-commands.jsonl'),
    }, null, 2) + '\n', { flag: 'wx', mode: 0o600 });
    console.log(JSON.stringify({ created: values.config, startedAgent: false }));
    return;
  }
  if (values.scode || values.workspace || values['session-id']) throw new Error('init fields require --init');
  // The standalone package changes only the launch surface. Ownership, policy,
  // wire handling and callback behavior stay in the existing shared host.
  installCommandShutdownSignals();
  if(values.codex && values['codex-owner'])throw new Error('Select one Codex entry mode');
  if(values['codex-owner'])await runCodexMcpOwner(values.config);
  else if(values.codex)await runCodexMcp(values.config,__filename,values['registration-id']);
  else await runScodeMcp(values.config);
}

/** Claude Code writes the hook's JSON input to stdin and closes it. */
function hookInput(): Promise<string> {
  if (process.stdin.isTTY) return Promise.resolve('');
  return new Promise((resolve, reject) => {
    let text = '';
    process.stdin.setEncoding('utf8');
    process.stdin.on('data', chunk => { text += chunk; });
    process.stdin.once('end', () => resolve(text));
    process.stdin.once('error', reject);
  });
}

void main().catch(error => { console.error(String(error)); process.exitCode = 1; });
