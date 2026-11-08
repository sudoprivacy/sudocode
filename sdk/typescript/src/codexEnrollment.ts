import { closeSync, fsyncSync, lstatSync, mkdirSync, openSync, readFileSync, writeFileSync } from 'node:fs';
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

function grantFor(file: string, claim: CodexAgentClaim): {grant: OperatorGrant; grantSha256: string} {
  const bytes=recordBytes(file);const grant=JSON.parse(bytes.toString('utf8')) as OperatorGrant;
  validateGrant(grant);
  if(!claim || claim.version!==1 || Object.keys(claim).sort().join(',')!=='actor,operationId,threadId,version,workspace'
      || !/^codex-[0-9a-f]{32}$/.test(claim.actor) || !identifier(claim.operationId) || !identifier(claim.threadId))throw new Error('Invalid Codex enrollment claim');
  canonicalWorkspace(claim.workspace);
  // No Node/CA private key or install authority is copied into this response.
  return {grant,grantSha256:createHash('sha256').update(bytes).digest('hex')};
}

/** Operator implementation of enrollmentArgv. Called only after an owner has
 * installed explicit policy and the native Node enrollment capability. It is
 * source/test preparation until installed; it never starts or installs a Node.
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
  const intent={claim,grantSha256,nativeCanonicalWorkspace:canonicalWorkspace(claim.workspace)};
  try {createRecord(operation,intent);}
  catch(error){
    if((error as NodeJS.ErrnoException).code!=='EEXIST')throw error;
    const previous=readJson(operation) as typeof intent;
    if(!isDeepStrictEqual(previous,intent))throw new Error('Actor operation/grant differs; operator recovery required');
    // No RPC/session retry, including after a crash before the first submission.
    // Only an already completed public receipt may be replayed.
    return readJson(resultFile) as CodexAgentProfile;
  }
  // This operation record is already reserved. Even a nested preflight refusal
  // cannot permit the outer bridge to replay the whole multi-stage workflow.
  const minted=await runEnrollmentCommand(grant.nexusArgv,claim).catch(error=>{
    throw new CodexEnrollmentError('unknown',String(error));
  }) as {
    version:number;actor:string;operationId:string;credentialDirectory:string;
  };
  if(!minted || Object.keys(minted).sort().join(',')!=='actor,credentialDirectory,operationId,version'
      || minted.version!==1 || minted.actor!==claim.actor || minted.operationId!==claim.operationId
      || typeof minted.credentialDirectory!=='string' || !path.isAbsolute(minted.credentialDirectory)) {
    throw new Error('Operator enrollment returned another operation or credential');
  }
  const credential=readJson(path.join(minted.credentialDirectory,'credential.json')) as {agent?:string;version?:number};
  if(credential.agent!==claim.actor || credential.version!==1)throw new Error('Minted credential is for another actor');
  const workspace=path.join(actorDirectory,'workspace');
  const settings=path.join(workspace,'.nexus','sudocode');
  mkdirSync(settings,{recursive:true,mode:0o700});
  createRecord(path.join(settings,'settings.json'),{agentName:claim.actor});
  const profile: ScodeAcpConfig={...grant.scode,workspace,
    protocolLog:path.join(actorDirectory,'acp.jsonl'),stderrLog:path.join(actorDirectory,'stderr.log')};
  const connection=await connectScode(profile);
  try {
    // Persist the actual native SID before any subsequent validation/close.
    createRecord(path.join(actorDirectory,'session.json'),{sessionId:connection.sessionId,workspace});
    const listed=await listScodeAgents(connection);
    requireScodeNodeIdentity(connection,listed,claim.actor);
    const configFile=path.join(actorDirectory,'profile.json');
    createRecord(configFile,{...profile,acpSessionId:connection.sessionId});
    await connection.close();
    const result: CodexAgentProfile={actor:claim.actor,configFile,workspaceBinding:{version:1,
      actor:claim.actor,operationId:claim.operationId,threadId:claim.threadId,
      nativeCanonicalWorkspace:intent.nativeCanonicalWorkspace,acpCanonicalWorkspace:canonicalWorkspace(workspace)}};
    createRecord(resultFile,result);
    return result;
  } finally {await connection.close();}
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
