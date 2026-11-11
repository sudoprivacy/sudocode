import { readFileSync, writeFileSync } from 'node:fs';
import * as path from 'node:path';
import { boundProfile, claimBinding, identifier, provisionBinding, readBindings } from './codexBindings';
import { CodexEnrollmentError } from './codexAutojoin';
import { enroll } from './codexMcp';
import { runScodeMcp, type ScodeMcpConfig } from './mcp';
import { registerCommandShutdown } from './lifecycle';

/**
 * Every Claude Code session joins the agent network as its own Agent, the way
 * each Codex thread does, with no launch flag. Claude Code starts one MCP server
 * per session and names the session in CLAUDE_CODE_SESSION_ID (stable across
 * --continue/--resume), so the session id is the native root: the first start
 * enrolls `claude-<32 hex>` through the operator's adapter, later starts reuse
 * the original Agent and ACP session. Peers reach the session through its wake
 * spool and the `--claude-wake` hook (asyncRewake), Claude Code's counterpart of
 * the `turn/start` a Codex attachment uses.
 */
export interface ClaudeAutojoinConfig {
  version: 1;
  mode: 'claude-autojoin';
  /** Private directory: bindings, per-session MCP configs and wake spools. */
  stateDirectory: string;
  auditLog: string;
  /** The same operator enrollment adapter Codex attachments use. */
  enrollmentArgv: string[];
  peerPolicy: 'allowlist' | 'node-namespace';
  peers?: string[];
}

export function readClaudeAutojoinConfig(file: string): ClaudeAutojoinConfig {
  const config = JSON.parse(readFileSync(path.resolve(file), 'utf8')) as ClaudeAutojoinConfig;
  if (!config || config.version !== 1 || config.mode !== 'claude-autojoin'
      || typeof config.stateDirectory !== 'string' || !path.isAbsolute(config.stateDirectory)
      || typeof config.auditLog !== 'string' || !path.isAbsolute(config.auditLog)
      || !Array.isArray(config.enrollmentArgv) || !config.enrollmentArgv.length
      || config.enrollmentArgv.some(arg => typeof arg !== 'string' || !arg || arg.includes('\0'))
      || !path.isAbsolute(config.enrollmentArgv[0])
      || !['allowlist', 'node-namespace'].includes(config.peerPolicy)
      || (config.peerPolicy === 'allowlist' && (!Array.isArray(config.peers) || !config.peers.length
        || config.peers.some(peer => typeof peer !== 'string' || !peer)))) {
    throw new Error('Claude autojoin config requires version:1, mode claude-autojoin, absolute stateDirectory/auditLog, '
      + 'an absolute enrollmentArgv and a peer policy');
  }
  return config;
}

/** The session's private spool, shared by its bridge and its wake hook. */
export function claudeWakeSpool(config: ClaudeAutojoinConfig, sessionId: string): string {
  if (!identifier(sessionId)) throw new Error('A Claude Code session id is required');
  return path.join(config.stateDirectory, `${sessionId}.wake.jsonl`);
}

export async function runClaudeMcp(file: string, environment: NodeJS.ProcessEnv = process.env, lifecycle: {
  registerShutdown?: typeof registerCommandShutdown;
} = {}): Promise<void> {
  const config = readClaudeAutojoinConfig(file);
  const sessionId = environment.CLAUDE_CODE_SESSION_ID ?? '';
  if (!identifier(sessionId)) throw new Error('--claude runs as a Claude Code MCP server; CLAUDE_CODE_SESSION_ID is missing');
  const workspace = environment.CLAUDE_PROJECT_DIR || process.cwd();
  // Durable bindings keep their own directory; the session's generated MCP
  // config and wake spool live beside it.
  const bindingDirectory = path.join(config.stateDirectory, 'bindings');
  const bindings = readBindings(bindingDirectory);
  const binding = await claimBinding(bindingDirectory, bindings, { id: sessionId, cwd: workspace, frontend: 'claude' });
  await provisionBinding(bindingDirectory, binding, workspace, claim => enroll(config.enrollmentArgv, claim),
    error => error instanceof CodexEnrollmentError && error.outcome === 'not_submitted');
  const fixed: ScodeMcpConfig = { ...boundProfile(binding), version: 1, auditLog: config.auditLog,
    claudeChannel: { peerPolicy: config.peerPolicy, ...(config.peers ? { peers: config.peers } : {}),
      wakeSpool: claudeWakeSpool(config, sessionId) } };
  const fixedFile = path.join(config.stateDirectory, `${sessionId}.mcp.json`);
  writeFileSync(fixedFile, JSON.stringify(fixed, null, 2) + '\n', { mode: 0o600 });
  await runScodeMcp(fixedFile, lifecycle);
}
