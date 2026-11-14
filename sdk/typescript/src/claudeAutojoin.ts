import { readFileSync } from 'node:fs';
import * as path from 'node:path';
import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import { boundProfile, claimBinding, identifier, provisionBinding, readBindings, settleInterruptedBindings } from './codexBindings';
import { retryableEnrollment } from './codexAutojoin';
import { enroll } from './codexMcp';
import { serveScodeMcp } from './mcp';
import { registerCommandShutdown } from './lifecycle';

/**
 * Every interactive Claude Code session joins the agent network as its own
 * Agent, the way each user root thread of Codex does, with no launch flag. Claude Code starts one MCP server
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
  /** Private directory: bindings and per-session wake spools. */
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

/** Claude Code's human-driven entrypoints. Print-mode, SDK, CI and hosted
 * sessions have no user to address, like a Codex thread that is not a user
 * root, so they run without an Agent. A missing or other entrypoint fails
 * closed. The MCP entry and the wake hook both decide with this predicate. */
const INTERACTIVE_ENTRYPOINTS = new Set(['cli', 'claude-vscode', 'claude-desktop']);

export function joinsAgentNetwork(environment: NodeJS.ProcessEnv): boolean {
  return INTERACTIVE_ENTRYPOINTS.has(environment.CLAUDE_CODE_ENTRYPOINT ?? '');
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
  if (!joinsAgentNetwork(environment)) {
    // Claude Code still starts the entry; a server without tools adds nothing.
    await new McpServer({ name: 'nexus-scode', version: '1.0.0' }).connect(new StdioServerTransport());
    return;
  }
  const sessionId = environment.CLAUDE_CODE_SESSION_ID ?? '';
  if (!identifier(sessionId)) throw new Error('--claude runs as a Claude Code MCP server; CLAUDE_CODE_SESSION_ID is missing');
  const workspace = environment.CLAUDE_PROJECT_DIR || process.cwd();
  const bindingDirectory = path.join(config.stateDirectory, 'bindings');
  const bindings = readBindings(bindingDirectory);
  await settleInterruptedBindings(bindingDirectory, bindings);
  const binding = await claimBinding(bindingDirectory, bindings, { id: sessionId, cwd: workspace, frontend: 'claude' });
  await serveScodeMcp({ auditLog: config.auditLog, claudeChannel: { peerPolicy: config.peerPolicy,
    ...(config.peers ? { peers: config.peers } : {}), wakeSpool: claudeWakeSpool(config, sessionId) } }, async () => {
    // A retryable failure keeps the binding pending, so the next attempt drives the same operation.
    await provisionBinding(bindingDirectory, binding, workspace, claim => enroll(config.enrollmentArgv, claim),
      retryableEnrollment);
    return { ...boundProfile(binding), version: 1, auditLog: config.auditLog };
  }, lifecycle);
}
