import * as assert from 'node:assert/strict';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { randomUUID } from 'node:crypto';
import { once } from 'node:events';
import { createServer } from 'node:net';
import { execFileSync } from 'node:child_process';
import { WebSocketServer } from 'ws';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';
import type { CodexAutojoin, CodexAgentClaim } from '../src/codexAutojoin';
const root = path.resolve(__dirname,'../..');
const {connectCodexAutojoin,sendScodeMessage,CodexEnrollmentError} = require(path.join(root,'dist/index.cjs')) as typeof import('../src/index');
const directory = fs.mkdtempSync(path.join(os.tmpdir(),'codex-autojoin-'));
const workspaceAlias = path.join(directory,'workspace-alias');
fs.symlinkSync(directory,workspaceAlias,'junction');
let nativeWorkspace = workspaceAlias;
const journal = path.join(directory,'acp.jsonl');
const calls: Array<{method:string}> = [];
const enrollments: CodexAgentClaim[] = [];
const ids = Array.from({length:7},()=>randomUUID());
let offline = true;
let loaded = ids.slice(0,4);
let owner: CodexAutojoin | undefined;
let mcp: Client | undefined;
let mcpOwnerPid: number | undefined;
let holdReads = false;
const pendingReads: Array<()=>void> = [];
const server = new WebSocketServer({host:'127.0.0.1',port:0});
const proxyEnv: Record<string,string> = {SCODE_NATIVE_ENV_SENTINEL:'owned-proxy'};
for(const [key,value] of Object.entries(process.env))if(value!==undefined && key!=='CODEX_THREAD_ID')proxyEnv[key]=value;
server.on('connection',socket=>socket.on('message',bytes=>{
  const call=JSON.parse(bytes.toString());calls.push(call);
  if(call.id===undefined)return;
  const thread={id:call.params?.threadId,cwd:nativeWorkspace,status:{type:'idle'},
    parentThreadId:null as string|null,threadSource:'user',ephemeral:false,canAcceptDirectInput:true};
  if(thread.id===ids[2]){thread.threadSource='subagent';thread.parentThreadId=ids[0];}
  if(thread.id===ids[3])thread.threadSource='guardian_review';
  const result=call.method==='initialize'?{userAgent:'fixture'}:call.method==='thread/loaded/list'
    ?{data:loaded,nextCursor:null}:call.method==='thread/read'?{thread}:{};
  const reply = () => {if(socket.readyState===1)socket.send(JSON.stringify({jsonrpc:'2.0',id:call.id,result}));};
  if(holdReads&&call.method==='thread/read')pendingReads.push(reply);else reply();
}));
const rows = () => fs.existsSync(journal)?fs.readFileSync(journal,'utf8').trim().split('\n').filter(Boolean).map(line=>JSON.parse(line)):[];
async function main() {
  await once(server,'listening');
  const address=server.address();assert.ok(address&&typeof address!=='string');
  const config={native:{codexArgv:[process.execPath,path.join(root,'test/fixtures/codex-stdio-proxy.mjs'),String(address.port),'normal'],
    socket:'fixture-native.sock',env:proxyEnv,closeTimeoutMs:600},stateDirectory:path.join(directory,'bindings'),peers:[],pollIntervalMs:100000,
    async resolveAgent(claim: CodexAgentClaim){
      enrollments.push(claim);
      if(claim.threadId===ids[4])throw new Error('Provider lost response after possible enrollment');
      if(claim.threadId===ids[6]&&offline)throw new CodexEnrollmentError('not_submitted','Node unavailable before enrollment submission');
      const file=path.join(directory,claim.actor+'.profile');
      fs.writeFileSync(file,JSON.stringify({scodeArgv:[process.execPath,path.join(root,'test/fixtures/scode-direct-a2a-agent.mjs')],
        workspace:claim.workspace,acpSessionId:'fixture-'+claim.operationId,closeTimeoutMs:300,
        env:{HYDRA_DIRECT_FIXTURE_JOURNAL:journal,SCODE_DIRECT_FIXTURE_ACTOR:claim.threadId===ids[5]?'another-actor':claim.actor},
      }),{flag:'wx',mode:0o600});
      return {actor:claim.actor,configFile:file};
    },
  };
  owner=await connectCodexAutojoin(config);
  assert.equal(owner.status().length,2,'subagents and feature workers never become root Agents');
  const first=owner.status();assert.equal(new Set(first.map(row=>row.actor)).size,2);
  assert.throws(()=>owner!.connectionFor(undefined),/metadata/);
  assert.throws(()=>owner!.connectionFor(ids[2]),/authenticated/);
  for(const row of first){
    const result=await sendScodeMessage(owner.connectionFor(row.threadId),{to:'fixture-peer',message:'exact 🧪\0',summary:'one append'});
    assert.equal(JSON.parse(result.content).routing.sender,row.actor);
  }
  const originalSession=owner.connectionFor(ids[0]).sessionId;
  loaded=[ids[1],ids[2],ids[3]];await owner.refresh();
  assert.throws(()=>owner!.connectionFor(ids[0]),/authenticated/);
  nativeWorkspace=fs.realpathSync.native(directory);
  loaded=ids.slice(0,4);await owner.refresh();
  assert.equal(owner.connectionFor(ids[0]).sessionId,originalSession);
  assert.equal(enrollments[0].workspace,workspaceAlias,'canonical lookup must preserve the original issuance input');
  assert.equal(enrollments.length,2,'unload/reload reuses the committed identity/profile');
  const originalBinding=JSON.parse(fs.readFileSync(path.join(config.stateDirectory,ids[0]+'.json'),'utf8'));
  const originalProfile=fs.readFileSync(originalBinding.configFile,'utf8');
  const replacedProfile=JSON.parse(originalProfile);replacedProfile.acpSessionId='another-session-for-the-same-actor';
  fs.writeFileSync(originalBinding.configFile,JSON.stringify(replacedProfile));
  await owner.refresh();
  assert.throws(()=>owner!.connectionFor(ids[0]),/ACP session differs/,
    'a mutable same-actor profile cannot replace the original ACP session');
  fs.writeFileSync(originalBinding.configFile,originalProfile);
  loaded=loaded.filter(id=>id!==ids[0]);await owner.refresh();
  loaded.push(ids[0]);await owner.refresh();
  assert.equal(owner.connectionFor(ids[0]).sessionId,originalSession);
  assert.equal(enrollments.length,2,'restoring the original profile reuses its original claim');
  loaded.push(ids[4],ids[5]);await owner.refresh();
  assert.equal(owner.status().find(row=>row.threadId===ids[4])?.state,'recovery_required');
  assert.throws(()=>owner!.connectionFor(ids[4]),/lost response/);
  assert.throws(()=>owner!.connectionFor(ids[5]),/does not match/);
  const beforeClose=owner.status();
  await owner.close();
  owner=await connectCodexAutojoin(config);
  const identities = (rows: ReturnType<CodexAutojoin['status']>) => rows.map(row=>[row.threadId,row.actor]).sort((a,b)=>a[0].localeCompare(b[0]));
  assert.deepEqual(identities(owner.status()),identities(beforeClose),
    'restart must keep every original Agent, including uncertain operations');
  assert.equal(owner.connectionFor(ids[0]).sessionId,originalSession);
  assert.equal(enrollments.length,4,'restart does not retry an ambiguous issuance or remint a failed actor');
  assert.equal(owner.status().find(row=>row.threadId===ids[4])?.state,'recovery_required');
  await owner.refresh();assert.equal(enrollments.length,4);
  loaded.push(ids[6]);await owner.refresh();
  const pending=owner.status().find(row=>row.threadId===ids[6]);
  assert.equal(pending?.state,'waiting_for_identity');
  const pendingClaim=enrollments.at(-1);
  offline=false;
  await new Promise(resolve=>setTimeout(resolve,2100));await owner.refresh();
  assert.equal(owner.status().find(row=>row.threadId===ids[6])?.state,'connected');
  assert.deepEqual(enrollments.at(-1),pendingClaim,'definite no-submission recovery keeps the original operation/actor');
  assert.equal(rows().filter(row=>row.method==='_sudocode/a2a/send').length,2,'no replay on restart');
  assert.ok(!rows().some(row=>['session/new','session/prompt'].includes(row.method)));
  assert.ok(!calls.some(row=>['thread/start','thread/resume','turn/start'].includes(row.method)));
  // Accept an OS path alias, but never adopt its new target after it moves.
  const beforeRetarget=enrollments.length;
  const otherWorkspace=path.join(directory,'another-workspace');fs.mkdirSync(otherWorkspace);
  nativeWorkspace=otherWorkspace;await owner.refresh();
  assert.throws(()=>owner!.connectionFor(ids[0]),/workspace differs/,'cached connections must not bypass workspace binding');
  nativeWorkspace=fs.realpathSync.native(directory);
  loaded=loaded.filter(id=>id!==ids[0]&&id!==ids[1]);await owner.refresh();
  loaded.push(ids[0],ids[1]);await owner.refresh();
  assert.equal(owner.connectionFor(ids[0]).sessionId,originalSession);
  loaded=loaded.filter(id=>id!==ids[0]);await owner.refresh();
  fs.unlinkSync(workspaceAlias);fs.symlinkSync(otherWorkspace,workspaceAlias,'junction');
  loaded.push(ids[0]);await owner.refresh();
  assert.throws(()=>owner!.connectionFor(ids[0]),/workspace differs/);
  loaded=loaded.filter(id=>id!==ids[0]);await owner.refresh();
  fs.unlinkSync(workspaceAlias);fs.symlinkSync(directory,workspaceAlias,'junction');
  loaded.push(ids[0]);await owner.refresh();
  assert.equal(owner.connectionFor(ids[0]).sessionId,originalSession);
  assert.equal(enrollments.length,beforeRetarget,'path recovery cannot re-enroll or change the operation');
  await owner.close();owner=undefined;
  const bindingFile=path.join(config.stateDirectory,ids[0]+'.json');
  const legacy=JSON.parse(fs.readFileSync(bindingFile,'utf8'));delete legacy.canonicalWorkspace;
  fs.writeFileSync(bindingFile,JSON.stringify(legacy));
  await assert.rejects(connectCodexAutojoin(config),/canonical workspace; operator migration required/);
  assert.equal(enrollments.length,beforeRetarget);

  // A resumed root can become visible before the owner's periodic inventory.
  // Its first real MCP call must wait for attachment, before any send begins.
  loaded=[];
  const reservation=createServer();reservation.listen(0,'127.0.0.1');await once(reservation,'listening');
  const reserved=reservation.address();assert.ok(reserved&&typeof reserved!=='string');
  await new Promise<void>(resolve=>reservation.close(()=>resolve()));
  const mcpConfig=path.join(directory,'mcp.json');
  fs.writeFileSync(mcpConfig,JSON.stringify({version:1,mode:'codex-autojoin',native:config.native,
    stateDirectory:path.join(directory,'mcp-bindings'),auditLog:path.join(directory,'mcp-audit.jsonl'),peers:[],
    enrollmentArgv:[process.execPath,path.join(root,'test/fixtures/codexAgentEnrollment.cjs'),directory],
    endpoint:{port:reserved.port,token:'01'.repeat(32)},
  }),{mode:0o600});
  mcp=new Client({name:'autojoin-process-acceptance',version:'1'},{capabilities:{}});
  await mcp.connect(new StdioClientTransport({command:process.execPath,args:[path.join(root,'dist/server.cjs'),
    '--codex','--config',mcpConfig],stderr:'pipe'}));
  const readyDeadline=Date.now()+10000;
  for(;;){
    const status=await mcp.callTool({name:'scode_status',arguments:{}});
    const content=status.content as Array<{type:string;text:string}>;
    if(JSON.parse(content[0].text).state==='observing')break;
    assert.ok(Date.now()<readyDeadline,'MCP owner did not attach its native observer');
    await new Promise(resolve=>setTimeout(resolve,25));
  }
  holdReads=true;loaded=[ids[0]];
  const sending=mcp.callTool({name:'scode_send',arguments:{to:'fixture-peer',message:'first resumed send',summary:'one append'},
    _meta:{threadId:ids[0]}});
  const attachmentDeadline=Date.now()+3000;
  while(!pendingReads.length){
    assert.ok(Date.now()<attachmentDeadline,'MCP call did not resolve the newly loaded root');
    await new Promise(resolve=>setTimeout(resolve,25));
  }
  assert.equal(await Promise.race([sending.then(()=>true),new Promise(resolve=>setTimeout(()=>resolve(false),100))]),false,
    'first MCP call must wait for its root attachment');
  assert.equal(rows().filter(row=>row.method==='_sudocode/a2a/send').length,2,'nothing appended while attachment was held');
  holdReads=false;for(const reply of pendingReads.splice(0))reply();
  assert.equal((await sending).isError,false);
  assert.equal(rows().filter(row=>row.method==='_sudocode/a2a/send').length,3,'resumed command appends exactly once');
  assert.ok(!rows().some(row=>['session/new','session/prompt'].includes(row.method)));
  const legacyId=randomUUID();
  const legacyProfile=path.join(directory,'legacy-profile.json');
  const legacySession='original-existing-acp-session';
  fs.writeFileSync(legacyProfile,JSON.stringify({scodeArgv:[process.execPath,path.join(root,'test/fixtures/scode-direct-a2a-agent.mjs')],
    workspace:otherWorkspace,acpSessionId:legacySession,closeTimeoutMs:300,
    env:{HYDRA_DIRECT_FIXTURE_JOURNAL:journal,SCODE_DIRECT_FIXTURE_ACTOR:'existing-agent'},
  }));
  const mappingFile=path.join(directory,'existing-mapping.json');
  fs.writeFileSync(mappingFile,JSON.stringify({threadId:legacyId,workspace:directory,actor:'existing-agent',
    configFile:legacyProfile,acpSessionId:legacySession}));
  const importArgs=[path.join(root,'dist/server.cjs'),'--bind-codex','--config',mcpConfig,'--binding',mappingFile];
  const prepare=()=>execFileSync(process.execPath,importArgs,{encoding:'utf8',stdio:['ignore','pipe','pipe']});
  const beforeImport=rows().length;
  assert.throws(prepare,/owner is already running/,'preparation shares the running owner exclusion');
  assert.equal(rows().length,beforeImport,'refused preparation cannot start another receiver');
  const status=await mcp.callTool({name:'scode_status',arguments:{}});
  mcpOwnerPid=JSON.parse((status.content as Array<{text:string}>)[0].text).ownerPid;
  assert.ok(mcpOwnerPid);
  await mcp.close();mcp=undefined;
  await new Promise(resolve=>setTimeout(resolve,6000));
  assert.doesNotThrow(()=>process.kill(mcpOwnerPid!,0),'loaded native roots keep their owner after MCP clients disconnect');
  loaded=[];
  const retirement=Date.now()+12000;
  for(;;){
    try {process.kill(mcpOwnerPid!,0);}
    catch(error){assert.equal((error as NodeJS.ErrnoException).code,'ESRCH');break;}
    assert.ok(Date.now()<retirement,'owner did not retire after authoritative root unload');
    await new Promise(resolve=>setTimeout(resolve,50));
  }
  mcpOwnerPid=undefined;
  const afterRetirement=rows().length;
  const configBytes=fs.readFileSync(mcpConfig);
  const imported=JSON.parse(prepare());
  assert.equal(imported.startedAgent,false);assert.equal(imported.acpSessionId,legacySession);
  assert.deepEqual(fs.readFileSync(mcpConfig),configBytes);
  assert.equal(rows().length,afterRetirement,'offline import performs no ACP operation');
  const frozenFile=path.join(directory,'mcp-bindings',legacyId+'.json');
  const frozenBytes=fs.readFileSync(frozenFile);
  assert.throws(prepare,/cannot be overwritten/);
  assert.deepEqual(fs.readFileSync(frozenFile),frozenBytes);
  const claimsBeforeImport=enrollments.length;
  loaded=[legacyId];
  owner=await connectCodexAutojoin({...config,stateDirectory:path.join(directory,'mcp-bindings')});
  assert.equal(owner.connectionFor(legacyId).sessionId,legacySession);
  assert.equal(owner.status()[0].actor,'existing-agent');
  assert.equal(enrollments.length,claimsBeforeImport,'existing import cannot enroll or remint');
  const sent=await sendScodeMessage(owner.connectionFor(legacyId),{to:'fixture-peer',message:'legacy original',summary:'original identity'});
  assert.equal(JSON.parse(sent.content).routing.sender,'existing-agent');
  const profileBytes=fs.readFileSync(legacyProfile);
  const changed=JSON.parse(profileBytes.toString());changed.workspace=directory;
  fs.writeFileSync(legacyProfile,JSON.stringify(changed));await owner.refresh();
  assert.throws(()=>owner!.connectionFor(legacyId),/ACP workspace differs/);
  fs.writeFileSync(legacyProfile,profileBytes);loaded=[];await owner.refresh();loaded=[legacyId];await owner.refresh();
  assert.equal(owner.connectionFor(legacyId).sessionId,legacySession);
  await owner.close();owner=await connectCodexAutojoin({...config,stateDirectory:path.join(directory,'mcp-bindings')});
  assert.equal(owner.connectionFor(legacyId).sessionId,legacySession);
  assert.equal(enrollments.length,claimsBeforeImport);
  assert.equal(rows().filter(row=>row.method==='_sudocode/a2a/send').length,4);
  assert.ok(!rows().some(row=>['session/new','session/prompt'].includes(row.method)));
  console.log('PASS native autojoin processes: root filtering, distinct identities, original SID pinning, independent frozen workspaces, exclusive offline import, original actor/session restart, owner retention/unload retirement, first-MCP readiness and no inference/replay');
}
void main().catch(error=>{console.error(error);process.exitCode=1;}).finally(async()=>{
  loaded=[];
  holdReads=false;for(const reply of pendingReads.splice(0))reply();
  await owner?.close();
  await mcp?.close();
  for(const client of server.clients)client.terminate();
  await new Promise<void>(resolve=>server.close(()=>resolve()));
  fs.rmSync(directory,{recursive:true,force:true});
});
