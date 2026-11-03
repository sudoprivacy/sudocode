import { execFile } from 'node:child_process';
import { createHash, randomUUID } from 'node:crypto';
import { closeSync, fsyncSync, lstatSync, openSync, readFileSync, readlinkSync, realpathSync,
  renameSync, unlinkSync, writeFileSync } from 'node:fs';
import * as path from 'node:path';
import { promisify } from 'node:util';
import { codexMcpInstallationCommands, prepareWindowsCodexOwner } from './codexMcp';

const execute=promisify(execFile);
const digest=(bytes:Buffer|string)=>createHash('sha256').update(bytes).digest('hex');
type EntrySnapshot={kind:'absent'}|{kind:'symlink';target:string;sha256:string}|{kind:'file';bytes:string;mode:number;sha256:string};

function snapshot(file:string):EntrySnapshot {
  let metadata;
  try{metadata=lstatSync(file);}catch(error){if((error as NodeJS.ErrnoException).code==='ENOENT')return {kind:'absent'};throw error;}
  if(process.getuid && metadata.uid!==process.getuid())throw new Error('The selected Codex command belongs to another user');
  if(metadata.isSymbolicLink()){
    const target=readlinkSync(file);return {kind:'symlink',target,sha256:digest(target)};
  }
  if(!metadata.isFile() || metadata.size>65536)throw new Error('Select a Codex launcher, not a native binary or directory');
  const bytes=readFileSync(file);
  if(bytes.includes(0))throw new Error('A native binary cannot be replaced by a launcher');
  return {kind:'file',bytes:bytes.toString('base64'),mode:metadata.mode&0o777,sha256:digest(bytes)};
}

function sameEntry(a:EntrySnapshot,b:EntrySnapshot):boolean {
  return a.kind===b.kind && (a.kind==='absent' || ('sha256' in b && a.sha256===b.sha256))
    && (a.kind!=='file' || (b.kind==='file' && a.mode===b.mode));
}

function quote(value:string):string {return "'"+value.replace(/'/g,"'\\''")+"'";}
function psQuote(value:string):string {return "'"+value.replace(/'/g,"''")+"'";}

function launcher(file:string,entry:string,target:string):Buffer {
  const argv=[process.execPath,entry,'--launch-codex','--config',file,'--'];
  if(argv.some(value=>/[\0\r\n]/.test(value)))throw new Error('Launcher paths cannot contain control characters');
  if(process.platform==='win32'){
    if(path.extname(target).toLowerCase()!=='.ps1')throw new Error('Select the ordinary PowerShell codex.ps1 launcher on Windows');
    return Buffer.from('& '+argv.map(psQuote).join(' ')+' @args\r\nexit $LASTEXITCODE\r\n');
  }
  return Buffer.from('#!/bin/sh\nexec '+argv.map(quote).join(' ')+' "$@"\n');
}

function save(file:string,value:unknown,exclusive=false):void {
  const fd=openSync(file,exclusive?'wx':'w',0o600);
  try{writeFileSync(fd,JSON.stringify(value,null,2)+'\n');fsyncSync(fd);}finally{closeSync(fd);}
}

async function command(argv:string[],env:Record<string,string>|undefined):Promise<{stdout:string;stderr:string}> {
  return execute(argv[0],argv.slice(1),{env,encoding:'utf8',shell:false,windowsHide:true,
    timeout:20000,killSignal:'SIGKILL',maxBuffer:65536});
}

async function registration(native:{codexArgv:string[];env?:Record<string,string>}):Promise<unknown|undefined> {
  try{return JSON.parse((await command([...native.codexArgv,'mcp','get','nexus_scode','--json'],native.env)).stdout);}
  catch(error){
    const failed=error as {code?:unknown;stderr?:unknown;stdout?:unknown};
    if(failed.code===1 && typeof failed.stderr==='string'
        && /^\s*(?:Error:\s*)?No MCP server named ['"]nexus_scode['"] found\.?\s*$/.test(failed.stderr)
        && !String(failed.stdout??'').trim())return undefined;
    throw new Error('Cannot establish the existing native MCP registration; installation has not been authorized by absence',{cause:error});
  }
}

function matchesRegistration(value:unknown,file:string,entry:string):boolean {
  const candidate=value as {name?:unknown;enabled?:unknown;transport?:{type?:unknown;command?:unknown;args?:unknown;env?:unknown;env_vars?:unknown;cwd?:unknown}};
  const transport=candidate?.transport;
  return candidate?.name==='nexus_scode' && candidate.enabled!==false && transport?.type==='stdio'
    && transport.command===process.execPath
    && JSON.stringify(transport.args)===JSON.stringify([entry,'--codex','--config',file])
    && (transport.env==null || (typeof transport.env==='object' && !Object.keys(transport.env).length))
    && (transport.env_vars==null || (Array.isArray(transport.env_vars) && !transport.env_vars.length))
    && transport.cwd==null;
}

/** One installation of an explicitly prepared, operator-authorized machine.
 * Native Codex and the existing current-user Task own their usual lifecycles.
 * No enrollment, Node/CA provision, root migration or input injection occurs.
 */
export async function installCodexMcp(file:string,entry:string,target:string):Promise<{
  installed:true; configFile:string; commandEntry:string; journal:string;
  appliesTo:'new Codex launches'; currentTerminal:'not inferred'; a2aJourneyVerified:false;
}> {
  if(![file,entry,target].every(value=>path.isAbsolute(value) && !/[\0\r\n]/.test(value)))throw new Error('Absolute private runtime, bridge and ordinary command paths are required');
  if(!/^codex(?:\.ps1)?$/i.test(path.basename(target)))throw new Error('Select the ordinary codex command entry');
  const runtimeSha256=digest(readFileSync(file));
  const runtimeUnchanged=()=>{
    if(digest(readFileSync(file))!==runtimeSha256){
      throw new Error('The selected runtime changed during preparation; its independent edit is preserved');
    }
  };
  const plan=codexMcpInstallationCommands(file,entry);
  runtimeUnchanged();
  const nativeHome=plan.native.env?.CODEX_HOME;
  if(!nativeHome || !path.isAbsolute(nativeHome) || !lstatSync(nativeHome).isDirectory()){
    throw new Error('Pin the existing selected CODEX_HOME in native.env before installing; the native command and owner must select the same home');
  }
  // A delegate must use the retained official command, not the public entry it
  // is about to replace. Canonical executable/wrapper paths keep that boundary.
  const normalized=(value:string)=>process.platform==='win32'?path.resolve(value).toLowerCase():path.resolve(value);
  for(const value of plan.native.codexArgv.filter(value=>path.isAbsolute(value))){
    if(normalized(value)===normalized(target) || normalized(realpathSync.native(value))!==normalized(value)){
      throw new Error('Pin the independent real native executable/official wrapper before replacing its public command');
    }
  }
  const before=snapshot(target),bytes=launcher(file,entry,target);
  const parent=lstatSync(path.dirname(target));
  if(!parent.isDirectory())throw new Error('The selected command parent must be an existing directory');
  const existing=await registration(plan.native);
  runtimeUnchanged();
  if(existing!==undefined && !matchesRegistration(existing,file,entry)){
    throw new Error('An existing Nexus MCP registration selects another configuration; explicit migration is required');
  }
  const journal=file+'.installation.json';
  const claim=path.join(nativeHome,'.sudocode-a2a-installation.json');
  const claimBytes=JSON.stringify({version:1,operationId:randomUUID(),runtime:file,journal,commandEntry:target});
  const state:{version:number;phase:string;runtimeSha256:string;commandEntry:string;commandEntrySha256:string;
    backup:EntrySnapshot;completed:string[];inFlight?:string;error?:string;ownerTask?:ReturnType<typeof prepareWindowsCodexOwner>}={
    version:1,phase:'prepared',runtimeSha256,commandEntry:target,
    commandEntrySha256:digest(bytes),backup:before,completed:[]};
  // Create-only admission prevents another installer or an uncertain partial
  // operation from replaying native registration/task commands.
  try{save(journal,state,true);}catch(error){
    if((error as NodeJS.ErrnoException).code==='EEXIST')throw new Error('An installation record already exists; inspect that completed/partial attempt before another operation');
    throw error;
  }
  const unchanged=()=>{
    if(readFileSync(claim,'utf8')!==claimBytes)throw new Error('The selected native-home installation claim changed');
    if(digest(readFileSync(file))!==state.runtimeSha256)throw new Error('The selected runtime changed during installation');
    if(!sameEntry(before,snapshot(target)))throw new Error('The ordinary command changed during installation; its independent edit is preserved');
  };
  const step=async(name:string,argv:string[])=>{
    unchanged();state.phase='applying';state.inFlight=name;save(journal,state);
    const result=await command(argv,name==='ownerTaskRegistration'?undefined:plan.native.env);
    state.completed.push(name);delete state.inFlight;save(journal,state);return result;
  };
  let temporary:string|undefined;
  try{
    // One create-only record per selected native home fences different runtime
    // installers too. It is a setup journal, not a native-service owner/lease.
    writeFileSync(claim,claimBytes,{flag:'wx',mode:0o600});
    if(process.platform==='win32'){
      unchanged();const owner=prepareWindowsCodexOwner(file,entry);
      state.ownerTask=owner;save(journal,state);
      // Preparation and installation use the existing canonical owner action;
      // registration has no trigger and does not start the task.
      await step('ownerTaskRegistration',owner.installationArgv);
    }
    const native=await step('officialNativePreparation',plan.nativePreparationArgv);
    if(JSON.parse(native.stdout).status!=='running')throw new Error('Official native preparation did not report running');
    unchanged();
    const current=await registration(plan.native);
    if(current!==undefined && !matchesRegistration(current,file,entry))throw new Error('MCP registration changed during preparation; independent configuration is preserved');
    if(current===undefined)await step('globalMcpRegistration',plan.registrationArgv);
    if(!matchesRegistration(await registration(plan.native),file,entry))throw new Error('Native MCP registration readback differs from the selected machine');
    state.completed.push('globalMcpReadback');save(journal,state);
    unchanged();
    temporary=path.join(path.dirname(target),'.codex-a2a-'+randomUUID());
    const handle=openSync(temporary,'wx',0o755);
    try{writeFileSync(handle,bytes);fsyncSync(handle);}finally{closeSync(handle);}
    // Recheck at the actual mutation boundary, after every external command.
    unchanged();state.inFlight='ordinaryCommand';save(journal,state);
    renameSync(temporary,target);temporary=undefined;
    state.completed.push('ordinaryCommand');delete state.inFlight;state.phase='verifying';save(journal,state);
    if(digest(readFileSync(target))!==state.commandEntrySha256)throw new Error('The installed ordinary command changed before readback');
    if(!matchesRegistration(await registration(plan.native),file,entry))throw new Error('The selected global MCP registration changed before final readback');
    if(process.platform!=='win32'){
      const parent=openSync(path.dirname(target),'r');try{fsyncSync(parent);}finally{closeSync(parent);}
    }
    state.completed.push('finalReadback');state.phase='installed';save(journal,state);
    return {installed:true,configFile:file,commandEntry:target,journal,appliesTo:'new Codex launches',
      currentTerminal:'not inferred',a2aJourneyVerified:false};
  }catch(error){
    state.phase='stopped';state.error=error instanceof Error?error.message:String(error);save(journal,state);
    throw new Error(`Codex installation stopped at ${state.inFlight??state.completed.at(-1)??'preparation'}; inspect ${journal}. The original command backup and partial state are retained; no automatic retry or rollback was performed`,{cause:error});
  }finally{if(temporary)unlinkSync(temporary);}
}
