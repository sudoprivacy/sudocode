#!/usr/bin/env node
import { createHash } from 'node:crypto';
import * as fs from 'node:fs';
import * as path from 'node:path';
import { parseArgs } from 'node:util';
import { runScodeMcp } from './mcp';
import { shutdownCommand } from './lifecycle';

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
  } });
  if (values.help) {
    console.log('scode-acp-bridge --config /absolute/private/runtime.json\n'
      + 'scode-acp-bridge --verify | --version\n'
      + 'scode-acp-bridge --init --config FILE --scode EXECUTABLE --workspace DIRECTORY --session-id EXISTING_ID\n'
      + 'Uses the shared scode ACP/MCP implementation; no Hydra application, tmux or Nexus daemon is started.');
    return;
  }
  if (values.version) {
    console.log(JSON.stringify({ implementation: 'scode-acp-bridge', sourceCommit: manifest().sourceCommit }));
    return;
  }
  if (values.verify) { verify(); return; }
  if (!values.config || !path.isAbsolute(values.config)) throw new Error('--config must be an absolute path');
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
  await runScodeMcp(values.config);
}

void main().catch(error => { console.error(String(error)); process.exitCode = 1; });
