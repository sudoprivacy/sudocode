import { randomUUID } from 'node:crypto';
import { closeSync, fsyncSync, linkSync, mkdirSync, openSync, readFileSync, readdirSync,
  realpathSync, renameSync, unlinkSync, writeSync } from 'node:fs';
import * as path from 'node:path';
import type { ScodeAcpConfig } from './scodeAcp';

export interface CodexAgentClaim {
  version: 1;
  actor: string;
  operationId: string;
  threadId: string;
  workspace: string;
}

/** The installed operator may select a private ACP context without granting it
 * access to the native project. This is a trusted local response, not a claim
 * from a peer and not a signature or additional-directory permission.
 */
export interface CodexWorkspaceBinding {
  version: 1;
  actor: string;
  operationId: string;
  threadId: string;
  nativeCanonicalWorkspace: string;
  acpCanonicalWorkspace: string;
}

export interface CodexBinding extends Omit<CodexAgentClaim, 'version'> {
  version: 2;
  origin: 'enrollment' | 'existing';
  canonicalWorkspace: string;
  state: 'pending' | 'provisioning' | 'ready' | 'recovery_required';
  configFile?: string;
  acp?: { sessionId: string; workspace: string; canonicalWorkspace: string };
  workspaceBinding?: CodexWorkspaceBinding;
}

export function identifier(value: unknown): value is string {
  return typeof value === 'string' && /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(value);
}

export function canonicalWorkspace(value: unknown): string {
  if (typeof value !== 'string' || !path.isAbsolute(value)) throw new Error('An absolute workspace is required');
  return realpathSync.native(value);
}

function sessionId(value: unknown): value is string {
  return typeof value === 'string' && !!value && value === value.trim() && !value.includes('\0');
}

export function assertWorkspace(binding: CodexBinding, workspace: unknown): void {
  if (canonicalWorkspace(workspace) !== binding.canonicalWorkspace
      || canonicalWorkspace(binding.workspace) !== binding.canonicalWorkspace) {
    throw new Error('Native root workspace differs from its durable binding');
  }
}

export function readProfile(file: string): ScodeAcpConfig {
  return JSON.parse(readFileSync(file, 'utf8')) as ScodeAcpConfig;
}

/** Freeze the original ACP context before connecting; imports may have a
 * deliberately different workspace, while new enrollments retain their claim.
 */
export function bindProfile(binding: CodexBinding, profile: ScodeAcpConfig, workspaceBinding?: CodexWorkspaceBinding): void {
  if (!sessionId(profile.acpSessionId)) throw new Error('An explicit existing ACP session ID is required');
  const canonical = canonicalWorkspace(profile.workspace);
  if (workspaceBinding !== undefined) {
    assertWorkspaceBinding(binding, canonical, workspaceBinding);
    binding.workspaceBinding = workspaceBinding;
  }
  if (binding.origin === 'enrollment' && !binding.workspaceBinding && canonical !== binding.canonicalWorkspace) {
    throw new Error('Enrolled ACP workspace differs from its native workspace claim');
  }
  binding.acp = { sessionId: profile.acpSessionId, workspace: profile.workspace, canonicalWorkspace: canonical };
}

function assertWorkspaceBinding(binding: CodexBinding, acpWorkspace: string, proof: CodexWorkspaceBinding): void {
  if (binding.origin !== 'enrollment' || !proof || proof.version !== 1
      || Object.keys(proof).sort().join(',') !== 'acpCanonicalWorkspace,actor,nativeCanonicalWorkspace,operationId,threadId,version'
      || proof.actor !== binding.actor || proof.operationId !== binding.operationId || proof.threadId !== binding.threadId
      || proof.nativeCanonicalWorkspace !== binding.canonicalWorkspace || proof.acpCanonicalWorkspace !== acpWorkspace) {
    throw new Error('Operator workspace binding differs from the original claim or ACP workspace');
  }
}

export function boundProfile(binding: CodexBinding): ScodeAcpConfig {
  if (!binding.configFile || !binding.acp) throw new Error('Durable binding lacks its frozen ACP context');
  const profile = readProfile(binding.configFile);
  if (profile.acpSessionId !== binding.acp.sessionId) throw new Error('ACP session differs from its durable binding');
  if (canonicalWorkspace(profile.workspace) !== binding.acp.canonicalWorkspace
      || canonicalWorkspace(binding.acp.workspace) !== binding.acp.canonicalWorkspace) {
    throw new Error('ACP workspace differs from its durable binding');
  }
  if (binding.workspaceBinding) assertWorkspaceBinding(binding, binding.acp.canonicalWorkspace, binding.workspaceBinding);
  return profile;
}

export function writeBinding(directory: string, binding: CodexBinding, create = false): void {
  mkdirSync(directory, { recursive: true, mode: 0o700 });
  const file = path.join(directory, `${binding.threadId}.json`);
  const temporary = `${file}.${randomUUID()}.tmp`;
  const handle = openSync(temporary, 'wx', 0o600);
  try {
    try { writeSync(handle, JSON.stringify(binding) + '\n'); fsyncSync(handle); }
    finally { closeSync(handle); }
    // Exclusive initial publication cannot overwrite an existing mapping.
    if (create) linkSync(temporary, file);
    else renameSync(temporary, file);
    if (create) unlinkSync(temporary);
    if (process.platform !== 'win32') {
      const parent = openSync(directory, 'r');
      try { fsyncSync(parent); } finally { closeSync(parent); }
    }
  } finally {
    try { unlinkSync(temporary); }
    catch (error) { if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error; }
  }
}

export function readBindings(directory: string): Map<string, CodexBinding> {
  mkdirSync(directory, { recursive: true, mode: 0o700 });
  const result = new Map<string, CodexBinding>();
  const actors = new Set<string>();
  for (const file of readdirSync(directory).filter(file => file.endsWith('.json'))) {
    const binding = JSON.parse(readFileSync(path.join(directory, file), 'utf8')) as CodexBinding;
    if (typeof binding.canonicalWorkspace !== 'string' || !path.isAbsolute(binding.canonicalWorkspace)) {
      throw new Error(`Durable Codex binding lacks a canonical workspace; operator migration required: ${file}`);
    }
    if (binding.version !== 2) throw new Error(`Durable Codex binding requires explicit operator migration: ${file}`);
    if (!identifier(binding.threadId) || file !== `${binding.threadId}.json`
        || typeof binding.actor !== 'string' || !/^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/.test(binding.actor)
        || !identifier(binding.operationId)
        || typeof binding.workspace !== 'string' || !path.isAbsolute(binding.workspace) || actors.has(binding.actor)
        || !['enrollment', 'existing'].includes(binding.origin)
        || !['pending', 'provisioning', 'ready', 'recovery_required'].includes(binding.state)
        || (binding.state === 'ready' && (!binding.configFile || !path.isAbsolute(binding.configFile)
          || !binding.acp || !sessionId(binding.acp.sessionId) || !path.isAbsolute(binding.acp.workspace)
          || !path.isAbsolute(binding.acp.canonicalWorkspace)))
        || (binding.origin === 'existing' && binding.state !== 'ready')
        || (binding.origin === 'enrollment' && !binding.workspaceBinding && binding.acp && binding.acp.canonicalWorkspace !== binding.canonicalWorkspace)) {
      throw new Error(`Invalid or duplicate durable Codex binding: ${file}`);
    }
    if (binding.workspaceBinding) {
      if (!binding.acp) throw new Error(`Operator workspace binding lacks ACP context: ${file}`);
      assertWorkspaceBinding(binding, binding.acp.canonicalWorkspace, binding.workspaceBinding);
    }
    actors.add(binding.actor); result.set(binding.threadId, binding);
  }
  return result;
}
