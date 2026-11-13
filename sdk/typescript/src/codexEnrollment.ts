import { closeSync, existsSync, fsyncSync, lstatSync, mkdirSync, openSync, readdirSync, readFileSync, renameSync, rmSync, writeFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import * as path from 'node:path';
import { isDeepStrictEqual } from 'node:util';
import { connectScode, type ScodeAcpConfig } from './scodeAcp';
import { listScodeAgents, requireScodeNodeIdentity } from './scodeA2a';
import { CodexEnrollmentError, type CodexAgentProfile } from './codexAutojoin';
import { canonicalWorkspace, identifier, type CodexAgentClaim } from './codexBindings';
import { runEnrollmentCommand } from './enrollmentCommand';

interface OperatorGrant {
  version: 1;
  purpose: 'codex-a2a';
  /** A fixed operator-side Node-granted command; never supplied by a peer. */
  nexusArgv: string[];
  stateDirectory: string;
  /** Explicit owner-selected launch policy. No workspace or existing session. */
  scode: Pick<ScodeAcpConfig,'scodeArgv'|'env'|'startupTimeoutMs'|'closeTimeoutMs'>;
}

/** Write only local policy. This does not run the operator, issue an identity,
 * install a global MCP entry, or start a service.
 */
export function prepareCodexEnrollment(file: string, nexusArgv: string[], scode: OperatorGrant['scode'], entry: string): {
  prepared: true; issued: false; enrollmentArgv: string[];
} {
  if(!path.isAbsolute(file) || !path.isAbsolute(entry))throw new Error('Absolute operator config/entry paths are required');
  const grant: OperatorGrant={version:1,purpose:'codex-a2a',nexusArgv,
    stateDirectory:path.join(path.dirname(file),'operator-agents'),scode};
  validateGrant(grant);
  mkdirSync(path.dirname(file),{recursive:true,mode:0o700});
  createRecord(file,grant);
  return {prepared:true,issued:false,enrollmentArgv:[process.execPath,entry,'--enroll-codex','--config',file]};
}

function recordBytes(file: string): Buffer {
  if(!path.isAbsolute(file))throw new Error('An absolute regular operator record is required');
  const metadata=lstatSync(file);
  if(!metadata.isFile() || metadata.size>16384)throw new Error('An operator record must be regular and at most 16 KiB');
  const bytes=readFileSync(file);
  if(bytes.length>16384)throw new Error('Operator record exceeds 16 KiB');
  return bytes;
}

function readJson(file: string): unknown {return JSON.parse(recordBytes(file).toString('utf8'));}

/** Exclusive, synced publication. A partial record is retained for operator
 * recovery and is never mistaken for permission to create another session.
 */
function createRecord(file: string, value: unknown): void {
  const handle=openSync(file,'wx',0o600);
  try {writeFileSync(handle,JSON.stringify(value)+'\n');fsyncSync(handle);}
  finally {closeSync(handle);}
  syncParent(file);
}

/** Publish a record once; a resumed operation accepts the identical record it
 * already published and refuses any other. */
function settleRecord(file: string, value: unknown): void {
  try {createRecord(file,value);}
  catch(error){
    if((error as NodeJS.ErrnoException).code!=='EEXIST')throw error;
    if(!isDeepStrictEqual(readJson(file),JSON.parse(JSON.stringify(value)))){
      throw new Error(`${path.basename(file)} differs from this operation; operator recovery required`);
    }
  }
}

/** Publish an actor's scode profile. Its workspace, logs and ACP session belong to
 * the operation and must match a profile already published; its launch policy
 * follows the operator record, so a resumed operation replaces a stale one. */
function settleProfile(file: string, value: ScodeAcpConfig & {acpSessionId: string}): 'published'|'unchanged'|'replaced' {
  try {createRecord(file,value);return 'published';}
  catch(error){if((error as NodeJS.ErrnoException).code!=='EEXIST')throw error;}
  const previous=readJson(file) as Partial<typeof value>;
  const owned=(profile: Partial<typeof value>)=>[profile.workspace,profile.protocolLog,profile.stderrLog,profile.acpSessionId];
  if(!isDeepStrictEqual(owned(previous),owned(value))){
    throw new Error('profile.json belongs to another operation; operator recovery required');
  }
  if(isDeepStrictEqual(previous,JSON.parse(JSON.stringify(value))))return 'unchanged';
  const staged=`${file}.${process.pid}.tmp`;
  createRecord(staged,value);
  renameSync(staged,file);
  syncParent(file);
  return 'replaced';
}

/** The scode profile an actor launches with: the operation's own workspace, logs
 * and ACP session, under the operator record's launch policy in force now.
 * Ready bindings read it at every connection, so publishing it is how a launch
 * policy change reaches an enrolled actor. */
function actorProfile(grant: OperatorGrant, actorDirectory: string): ScodeAcpConfig {
  const workspace=path.join(actorDirectory,'workspace');
  return {...grant.scode,workspace,
    protocolLog:path.join(actorDirectory,'acp.jsonl'),stderrLog:path.join(actorDirectory,'stderr.log')};
}

/** Republish a completed actor's profile under the current launch policy. Its ACP
 * session comes from the operation's own session record. */
function refreshCompletedProfile(grant: OperatorGrant, actorDirectory: string, result: CodexAgentProfile): 'published'|'unchanged'|'replaced' {
  const configFile=path.join(actorDirectory,'profile.json');
  if(result.configFile!==configFile)throw new Error('Completed operation names a profile outside its actor directory; operator recovery required');
  const saved=readJson(path.join(actorDirectory,'session.json')) as {sessionId?:unknown;workspace?:unknown};
  const profile=actorProfile(grant,actorDirectory);
  if(typeof saved.sessionId!=='string' || !saved.sessionId || saved.workspace!==profile.workspace){
    throw new Error('Completed operation lacks its ACP session record; operator recovery required');
  }
  return settleProfile(configFile,{...profile,acpSessionId:saved.sessionId});
}

function removeRecord(file: string): void {
  rmSync(file);
  syncParent(file);
}

function syncParent(file: string): void {
  if(process.platform!=='win32'){
    const parent=openSync(path.dirname(file),'r');
    try {fsyncSync(parent);}finally {closeSync(parent);}
  }
}

function validateGrant(grant: OperatorGrant): void {
  if(!grant || grant.version!==1 || grant.purpose!=='codex-a2a'
      || Object.keys(grant).some(key=>!['version','purpose','nexusArgv','stateDirectory','scode'].includes(key))
      || !path.isAbsolute(grant.stateDirectory) || !Array.isArray(grant.nexusArgv) || !grant.nexusArgv.length
      || grant.nexusArgv.some(arg=>typeof arg!=='string'||!arg||arg.includes('\0')) || !path.isAbsolute(grant.nexusArgv[0])
      || !grant.scode || Object.keys(grant.scode).some(key=>!['scodeArgv','env','startupTimeoutMs','closeTimeoutMs'].includes(key))
      || !Array.isArray(grant.scode.scodeArgv) || !grant.scode.scodeArgv.length || !path.isAbsolute(grant.scode.scodeArgv[0])
      || grant.scode.scodeArgv.some(arg=>typeof arg!=='string'||arg.includes('\0'))
      || (grant.scode.env!==undefined && (typeof grant.scode.env!=='object' || grant.scode.env===null
        || Object.entries(grant.scode.env).some(([key,value])=>!key || key.includes('=') || key.includes('\0')
          || (value!==null && (typeof value!=='string' || value.includes('\0'))))))
      || [grant.scode.startupTimeoutMs,grant.scode.closeTimeoutMs].some(value=>value!==undefined && (!Number.isFinite(value)||value<=0))) {
    throw new Error('Invalid explicit Codex enrollment grant');
  }
}

/** What one operation is, as its reservation records it.
 *
 * A replay must be the same operation: the same claim, for the same workspace. While
 * it is in flight it must also be driven by the same Node operator command, the one
 * thing that can issue a credential for it. The scode launch policy is not part of
 * it: a completed operation's credential and session do not depend on it, and a
 * resumed one starts scode with the policy in force now — so upgrading scode or
 * tuning its timeouts in the operator record strands no operation.
 */
interface OperationRecord {
  version: 2;
  claim: CodexAgentClaim;
  nativeCanonicalWorkspace: string;
  nexusArgv: string[];
}

/** A reservation written before {@link OperationRecord}: it pinned the whole
 * operator record by hash, so the issuer is only recognisable by that hash. */
interface LegacyOperationRecord {
  claim: CodexAgentClaim;
  grantSha256: string;
  nativeCanonicalWorkspace: string;
}

/** Why an existing reservation cannot be resumed by this call, or undefined when
 * it can. A completed operation is resumable under any operator record: nothing
 * further is issued for it. */
function resumeRefusal(previous: OperationRecord|LegacyOperationRecord, intent: OperationRecord,
  grantSha256: string, completed: boolean): string|undefined {
  if(!isDeepStrictEqual(previous.claim,intent.claim) || previous.nativeCanonicalWorkspace!==intent.nativeCanonicalWorkspace){
    return 'Another operation holds this actor; operator recovery required';
  }
  if(completed)return undefined;
  const sameIssuer='version' in previous && previous.version===2
    ? isDeepStrictEqual(previous.nexusArgv,intent.nexusArgv)
    : (previous as LegacyOperationRecord).grantSha256===grantSha256;
  return sameIssuer ? undefined
    : 'This operation was started by another Node operator command and may have been submitted; operator recovery required';
}

function grantFor(file: string, claim: CodexAgentClaim): {grant: OperatorGrant; grantSha256: string} {
  const bytes=recordBytes(file);const grant=JSON.parse(bytes.toString('utf8')) as OperatorGrant;
  validateGrant(grant);
  if(!claim || claim.version!==1 || Object.keys(claim).sort().join(',')!=='actor,operationId,threadId,version,workspace'
      || !/^(?:codex|claude)-[0-9a-f]{32}$/.test(claim.actor) || !identifier(claim.operationId) || !identifier(claim.threadId))throw new Error('Invalid Agent enrollment claim');
  canonicalWorkspace(claim.workspace);
  // No Node/CA private key or install authority is copied into this response.
  return {grant,grantSha256:createHash('sha256').update(bytes).digest('hex')};
}

/** Operator implementation of enrollmentArgv. Called only after an owner has
 * installed explicit policy and the native Node enrollment capability. It is
 * source/test preparation until installed; it never starts or installs a Node.
 *
 * One operation per actor, reserved before anything is issued. Every stage
 * after the reservation replays its durable record, so running the same
 * operation again resumes it: the Node operator replays a completed issuance
 * from its receipt and never resubmits one it may have sent, and the one ACP
 * session the operation created is loaded again, never replaced.
 */
export async function enrollCodexAgent(file: string, claim: CodexAgentClaim): Promise<CodexAgentProfile> {
  let prepared: ReturnType<typeof grantFor>;
  try {prepared=grantFor(file,claim);}
  catch(error){throw new CodexEnrollmentError('not_submitted',String(error));}
  const {grant,grantSha256}=prepared;
  mkdirSync(grant.stateDirectory,{recursive:true,mode:0o700});
  const actorDirectory=path.join(grant.stateDirectory,claim.actor);
  mkdirSync(actorDirectory,{recursive:true,mode:0o700});
  const operation=path.join(actorDirectory,'operation.json');
  const resultFile=path.join(actorDirectory,'result.json');
  const intent: OperationRecord={version:2,claim,nativeCanonicalWorkspace:canonicalWorkspace(claim.workspace),
    nexusArgv:grant.nexusArgv};
  try {createRecord(operation,intent);}
  catch(error){
    if((error as NodeJS.ErrnoException).code!=='EEXIST')throw error;
    const completed=existsSync(resultFile);
    const refusal=resumeRefusal(readJson(operation) as OperationRecord|LegacyOperationRecord,intent,grantSha256,completed);
    if(refusal)throw new Error(refusal);
    if(completed){
      const result=readJson(resultFile) as CodexAgentProfile;
      refreshCompletedProfile(grant,actorDirectory,result);
      return result;
    }
  }
  // The Node operator's typed refusal attests that nothing was submitted, so the
  // reservation is released and the same operation may run again. Any other
  // failure may have issued a credential and keeps the reservation.
  const minted=await runEnrollmentCommand(grant.nexusArgv,claim).catch(error=>{
    if(error instanceof CodexEnrollmentError && error.outcome==='not_submitted'){
      removeRecord(operation);
      throw error;
    }
    throw new CodexEnrollmentError('unknown',String(error));
  }) as {
    version:number;actor:string;operationId:string;credentialDirectory:string;
  };
  if(!minted || Object.keys(minted).sort().join(',')!=='actor,credentialDirectory,operationId,version'
      || minted.version!==1 || minted.actor!==claim.actor || minted.operationId!==claim.operationId
      || typeof minted.credentialDirectory!=='string' || !path.isAbsolute(minted.credentialDirectory)) {
    throw new CodexEnrollmentError('unknown','Operator enrollment returned another operation or credential');
  }
  const credential=readJson(path.join(minted.credentialDirectory,'credential.json')) as {agent?:string;version?:number};
  if(credential.agent!==claim.actor || credential.version!==1){
    throw new CodexEnrollmentError('unknown','Minted credential is for another actor');
  }
  // The credential is issued. The remaining stages are local and replay their
  // records, so any failure among them resumes the same operation.
  try {return await completeEnrollment(grant,claim,intent.nativeCanonicalWorkspace,actorDirectory,resultFile);}
  catch(error){
    throw error instanceof CodexEnrollmentError ? error : new CodexEnrollmentError('resumable',String(error));
  }
}

async function completeEnrollment(grant: OperatorGrant, claim: CodexAgentClaim, nativeCanonicalWorkspace: string,
  actorDirectory: string, resultFile: string): Promise<CodexAgentProfile> {
  const profile=actorProfile(grant,actorDirectory);
  const workspace=path.join(actorDirectory,'workspace');
  const settings=path.join(workspace,'.nexus','sudocode');
  mkdirSync(settings,{recursive:true,mode:0o700});
  settleRecord(path.join(settings,'settings.json'),{agentName:claim.actor});
  const sessionFile=path.join(actorDirectory,'session.json');
  const saved=existsSync(sessionFile) ? readJson(sessionFile) as {sessionId:string;workspace:string} : undefined;
  if(saved && (saved.workspace!==workspace || typeof saved.sessionId!=='string' || !saved.sessionId)){
    throw new CodexEnrollmentError('unknown','Persisted ACP session belongs to another operation; operator recovery required');
  }
  const connection=await connectScode(saved ? {...profile,acpSessionId:saved.sessionId} : profile);
  try {
    // Persist the actual native SID before any subsequent validation/close.
    if(!saved)createRecord(sessionFile,{sessionId:connection.sessionId,workspace});
    const listed=await listScodeAgents(connection);
    requireScodeNodeIdentity(connection,listed,claim.actor);
    const configFile=path.join(actorDirectory,'profile.json');
    settleProfile(configFile,{...profile,acpSessionId:connection.sessionId});
    await connection.close();
    const result: CodexAgentProfile={actor:claim.actor,configFile,workspaceBinding:{version:1,
      actor:claim.actor,operationId:claim.operationId,threadId:claim.threadId,
      nativeCanonicalWorkspace,acpCanonicalWorkspace:canonicalWorkspace(workspace)}};
    settleRecord(resultFile,result);
    return result;
  } finally {await connection.close();}
}

export interface ProfileRefresh {actor: string; outcome: 'published'|'unchanged'|'replaced'|'in_flight'|'refused'; detail?: string}

/** Republish every completed actor's profile under the operator record's current
 * launch policy, so a scode upgrade or a timeout change reaches actors that are
 * already enrolled. It writes profiles only: no operation, result, session or
 * credential record changes, nothing is issued, and an operation still in flight
 * picks the policy up when it resumes. Ready bindings use a refreshed profile
 * from their next connection. */
export function refreshCodexProfiles(file: string): ProfileRefresh[] {
  const grant=JSON.parse(recordBytes(file).toString('utf8')) as OperatorGrant;
  validateGrant(grant);
  if(!existsSync(grant.stateDirectory))return [];
  return readdirSync(grant.stateDirectory,{withFileTypes:true})
    .filter(entry=>entry.isDirectory() && /^(?:codex|claude)-[0-9a-f]{32}$/.test(entry.name))
    .map(entry=>entry.name).sort()
    .map((actor): ProfileRefresh=>{
      const actorDirectory=path.join(grant.stateDirectory,actor);
      const resultFile=path.join(actorDirectory,'result.json');
      if(!existsSync(resultFile))return {actor,outcome:'in_flight'};
      try {
        const result=readJson(resultFile) as CodexAgentProfile;
        if(result.actor!==actor)throw new Error('Completed result names another actor; operator recovery required');
        return {actor,outcome:refreshCompletedProfile(grant,actorDirectory,result)};
      } catch(error){return {actor,outcome:'refused',detail:String(error)};}
    });
}

/** Public wire entry: all untyped post-attempt failures remain Unknown. */
export async function runCodexEnrollment(file: string): Promise<void> {
  try {
    let bytes=Buffer.alloc(0);
    for await(const chunk of process.stdin){
      bytes=Buffer.concat([bytes,Buffer.from(chunk)]);
      if(bytes.length>8192)throw new CodexEnrollmentError('not_submitted','Enrollment claim exceeds 8 KiB');
    }
    let claim: CodexAgentClaim;
    try {claim=JSON.parse(bytes.toString('utf8'));}
    catch(error){throw new CodexEnrollmentError('not_submitted',String(error));}
    process.stdout.write(JSON.stringify(await enrollCodexAgent(file,claim))+'\n');
  } catch(error){
    process.stdout.write(JSON.stringify({outcome:error instanceof CodexEnrollmentError?error.outcome:'unknown',detail:String(error)})+'\n');
  }
}
