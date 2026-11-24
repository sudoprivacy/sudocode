import * as fs from 'node:fs';
import * as path from 'node:path';
import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import { boundProfile, claimBinding, identifier, provisionBinding, readBindings, settleInterruptedBindings } from './codexBindings';
import { retryableEnrollment } from './codexAutojoin';
import { enroll } from './codexMcp';
import { serveScodeMcp } from './mcp';
import { registerCommandShutdown } from './lifecycle';
import { openClaudeDelivery } from './claudeDelivery';
import { takeClaudeDeliveries } from './claudeWake';

/**
 * Every interactive Claude Code session joins the agent network as its own
 * Agent, the way each user root thread of Codex does, with no launch flag. Claude Code starts one MCP server
 * per session and names the session in CLAUDE_CODE_SESSION_ID (stable across
 * --continue/--resume), so the session id is the native root: the first start
 * enrolls `claude-<32 hex>` through the operator's adapter, later starts reuse
 * the original Agent and ACP session. The bridge owns the session's mailbox
 * (peerInput v3) and appends budgeted deliveries to the session's spool; the
 * `--claude-watch` SessionStart and CwdChanged hook has Claude Code watch the
 * spool and the `--claude-deliver` FileChanged hook (asyncRewake) hands each
 * new delivery to Claude, the counterpart of the `thread/queue/add` a Codex
 * attachment uses.
 */
export interface ClaudeAutojoinConfig {
  version: 1;
  mode: 'claude-autojoin';
  /** Private directory: bindings, and per session its spool and delivery log. */
  stateDirectory: string;
  auditLog: string;
  /** The same operator enrollment adapter Codex attachments use. */
  enrollmentArgv: string[];
  /** Mailbox offers carry every sender the Node stamped; there is no allowlist. */
  peerPolicy: 'node-namespace';
}

export function readClaudeAutojoinConfig(file: string): ClaudeAutojoinConfig {
  return validateClaudeAutojoinConfig(JSON.parse(fs.readFileSync(path.resolve(file), 'utf8')) as ClaudeAutojoinConfig);
}

export function validateClaudeAutojoinConfig(config: ClaudeAutojoinConfig): ClaudeAutojoinConfig {
  if (!config || config.version !== 1 || config.mode !== 'claude-autojoin'
      || typeof config.stateDirectory !== 'string' || !path.isAbsolute(config.stateDirectory)
      || typeof config.auditLog !== 'string' || !path.isAbsolute(config.auditLog)
      || !Array.isArray(config.enrollmentArgv) || !config.enrollmentArgv.length
      || config.enrollmentArgv.some(arg => typeof arg !== 'string' || !arg || arg.includes('\0'))
      || !path.isAbsolute(config.enrollmentArgv[0]) || config.peerPolicy !== 'node-namespace') {
    throw new Error('Claude autojoin config requires version:1, mode claude-autojoin, absolute stateDirectory/auditLog, '
      + 'an absolute enrollmentArgv and peerPolicy node-namespace');
  }
  return config;
}

/** Claude Code's human-driven entrypoints. Print-mode, SDK, CI and hosted
 * sessions have no user to address, like a Codex thread that is not a user
 * root, so they run without an Agent. A missing or other entrypoint fails
 * closed. The MCP entry and both hooks decide with this predicate. */
const INTERACTIVE_ENTRYPOINTS = new Set(['cli', 'claude-vscode', 'claude-desktop']);

export function joinsAgentNetwork(environment: NodeJS.ProcessEnv): boolean {
  return INTERACTIVE_ENTRYPOINTS.has(environment.CLAUDE_CODE_ENTRYPOINT ?? '');
}

/** The session's private spool, shared by its bridge and its hooks. */
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
  const delivery = openClaudeDelivery({ stateDirectory: config.stateDirectory, sessionId, actor: binding.actor,
    spool: claudeWakeSpool(config, sessionId) });
  try {
    await serveScodeMcp({ auditLog: config.auditLog, claude: delivery }, async () => {
      // A retryable failure keeps the binding pending, so the next attempt drives the same operation.
      await provisionBinding(bindingDirectory, binding, workspace, claim => enroll(config.enrollmentArgv, claim),
        retryableEnrollment);
      const profile = boundProfile(binding);
      // The owned scode publishes this as the session's peer label.
      return { ...profile, env: { ...profile.env, SUDOCODE_CODEX_PEER_ATTACHMENT: JSON.stringify({
        actor: binding.actor, workspace: path.basename(workspace) || 'filesystem root', frontend: 'Claude Code' }) },
      version: 1, auditLog: config.auditLog };
    }, lifecycle);
  } finally { await delivery.close(); }
}

/** Claude Code's JSON hook input names the session and the hook event, and for FileChanged the path and event. */
function hookInput(text: string): { session_id?: unknown; hook_event_name?: unknown; file_path?: unknown; event?: unknown } {
  const value = JSON.parse(text || '{}') as unknown;
  return value && typeof value === 'object' ? value : {};
}

const samePath = (left: string, right: string) => {
  if (path.resolve(left) === path.resolve(right)) return true;
  try { return fs.realpathSync(left) === fs.realpathSync(right); } catch { return false; }
};

/** The hook events whose output names the spool in `watchPaths`. On every
 * working-directory change, including a shell that returns to the project
 * after a command ran elsewhere, Claude Code rebuilds the dynamic watch list
 * from the CwdChanged hooks' output alone and drops what SessionStart named. */
const WATCH_EVENTS = new Set(['SessionStart', 'CwdChanged']);

/** SessionStart and CwdChanged: the JSON that has Claude Code watch this
 * session's spool, or nothing for a session without an Agent. Local files
 * only: no network, no scode. */
export function claudeWatch(file: string, input: string, environment: NodeJS.ProcessEnv = process.env): string | undefined {
  const config = readClaudeAutojoinConfig(file);
  if (!joinsAgentNetwork(environment)) return undefined;
  const hook = hookInput(input);
  const event = hook.hook_event_name;
  if (typeof event !== 'string' || !WATCH_EVENTS.has(event)) {
    throw new Error('--claude-watch serves the SessionStart and CwdChanged hooks');
  }
  const spool = claudeWakeSpool(config, String(hook.session_id ?? ''));
  // The watched path exists before the bridge's first delivery.
  fs.mkdirSync(config.stateDirectory, { recursive: true, mode: 0o700 });
  fs.writeFileSync(spool, '', { flag: 'a', mode: 0o600 });
  return JSON.stringify({ hookSpecificOutput: { hookEventName: event, watchPaths: [spool] } });
}

/** FileChanged: the text of this session's new deliveries, each claimed by
 * exactly one run, or nothing for another path, a removal or no new entry. */
export function claudeDeliver(file: string, input: string, environment: NodeJS.ProcessEnv = process.env): string | undefined {
  const config = readClaudeAutojoinConfig(file);
  if (!joinsAgentNetwork(environment)) return undefined;
  const event = hookInput(input);
  const spool = claudeWakeSpool(config, String(event.session_id ?? ''));
  if (event.event === 'unlink' || typeof event.file_path !== 'string' || !samePath(event.file_path, spool)) return undefined;
  return takeClaudeDeliveries(spool);
}
