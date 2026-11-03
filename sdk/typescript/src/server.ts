#!/usr/bin/env node
import { createHash } from 'node:crypto';
import * as fs from 'node:fs';
import * as path from 'node:path';
import { parseArgs } from 'node:util';
import { runScodeMcp } from './mcp';
import { runCodexMcp, runCodexMcpOwner, prepareCodexMcp, prepareExistingCodexBinding, prepareWindowsCodexOwner } from './codexMcp';
import { shutdownCommand } from './lifecycle';
import { runCodexEnrollment, prepareCodexEnrollment } from './codexEnrollment';

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
  const { values } = parseArgs({ options: {
    config: { type: 'string' }, help: { type: 'boolean' }, version: { type: 'boolean' },
    verify: { type: 'boolean' }, init: { type: 'boolean' }, scode: { type: 'string' },
    workspace: { type: 'string' }, 'session-id': { type: 'string' },
    codex: { type: 'boolean' }, 'codex-owner': { type: 'boolean' },
    'prepare-codex-owner': { type: 'boolean' },
    'init-codex': {type:'boolean'}, 'native-codex': {type:'string'}, 'native-argv': {type:'string'},
    enrollment: {type:'string'}, peers: {type:'string'}, port: {type:'string'},
    'peer-policy': {type:'string'},
    'bind-codex': {type:'boolean'}, binding: {type:'string'},
    'enroll-codex': {type:'boolean'},
    'init-codex-operator': {type:'boolean'}, 'nexus-operator': {type:'string'}, 'scode-argv': {type:'string'}, 'scode-env': {type:'string'},
  } });
  if (values.help) {
    console.log('scode-acp-bridge --config /absolute/private/runtime.json\n'
      + 'scode-acp-bridge --verify | --version\n'
      + 'scode-acp-bridge --init --config FILE --scode EXECUTABLE --workspace DIRECTORY --session-id EXISTING_ID\n'
      + 'scode-acp-bridge --init-codex --config FILE --native-codex EXECUTABLE --enrollment JSON_ARGV --peers JSON_NAMES [--port 23262]\n'
      + '  Use --native-argv JSON_ARGV instead of --native-codex to preserve an official wrapper command.\n'
      + '  --peer-policy node-namespace explicitly accepts peer DATA from the verified Node namespace; default: allowlist.\n'
      + 'scode-acp-bridge --bind-codex --config FILE --binding EXISTING_MAPPING_FILE  (prepare only, while bridge is stopped)\n'
      + 'scode-acp-bridge --codex --config FILE  (global Codex MCP entry)\n'
      + 'scode-acp-bridge --prepare-codex-owner --config FILE  (prepare Windows current-user task; does not register/start it)\n'
      + 'scode-acp-bridge --enroll-codex --config OWNER_GRANT  (operator adapter, public JSON stdin)\n'
      + 'scode-acp-bridge --init-codex-operator --config FILE --nexus-operator JSON_ARGV --scode-argv JSON_ARGV [--scode-env JSON_OBJECT]\n'
      + 'Uses the shared scode ACP/MCP implementation; no Hydra application, tmux or Nexus daemon is started.');
    return;
  }
  if (values.version) {
    console.log(JSON.stringify({ implementation: 'scode-acp-bridge', sourceCommit: manifest().sourceCommit }));
    return;
  }
  if (values.verify) { verify(); return; }
  if (!values.config || !path.isAbsolute(values.config)) throw new Error('--config must be an absolute path');
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
  if(values['enroll-codex']){
    if(Object.entries(values).some(([key,value])=>value!==undefined && !['config','enroll-codex'].includes(key)))throw new Error('Select one operator enrollment mode');
    await runCodexEnrollment(values.config);return;
  }
  if(values['bind-codex']){
    if(values.init||values['init-codex']||values.codex||values['codex-owner']||values.scode||values.workspace
      ||values['session-id']||values['native-codex']||values['native-argv']||values.enrollment||values.peers||values.port||values['peer-policy'])throw new Error('Select one preparation mode');
    if(!values.binding||!path.isAbsolute(values.binding))throw new Error('--binding must be an absolute private mapping file');
    console.log(JSON.stringify(await prepareExistingCodexBinding(values.config,JSON.parse(fs.readFileSync(values.binding,'utf8')))));
    return;
  }
  if(values.binding)throw new Error('--binding requires --bind-codex');
  if(values['init-codex']){
    if(values.init||values.codex||values['codex-owner']||values.scode||values.workspace||values['session-id'])throw new Error('Select one initializer mode');
    if(Boolean(values['native-codex'])===Boolean(values['native-argv']))throw new Error('Select exactly one native executable or argv');
    console.log(JSON.stringify(prepareCodexMcp(values.config,values['native-argv']?JSON.parse(values['native-argv']):values['native-codex']!,JSON.parse(values.enrollment??'null'),
      JSON.parse(values.peers??'null'),__filename,values.port===undefined?undefined:Number(values.port),
      values['peer-policy'] as 'allowlist'|'node-namespace'|undefined)));
    return;
  }
  if(values['native-codex']||values['native-argv']||values.enrollment||values.peers||values.port||values['peer-policy'])throw new Error('Codex initializer fields require --init-codex');
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
  for (const [signal, exit] of [['SIGINT', 130], ['SIGTERM', 143]] as const) {
    process.once(signal, () => { void shutdownCommand().finally(() => process.exit(exit)); });
  }
  if(values.codex && values['codex-owner'])throw new Error('Select one Codex entry mode');
  if(values['codex-owner'])await runCodexMcpOwner(values.config);
  else if(values.codex)await runCodexMcp(values.config,__filename);
  else await runScodeMcp(values.config);
}

void main().catch(error => { console.error(String(error)); process.exitCode = 1; });
