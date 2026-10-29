import * as assert from 'node:assert/strict';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { randomUUID } from 'node:crypto';
import { once } from 'node:events';
import { WebSocketServer } from 'ws';
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
  socket.send(JSON.stringify({jsonrpc:'2.0',id:call.id,result}));
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
  console.log('PASS native autojoin processes: root filtering, distinct identities, exact routing, canonical workspace alias and retarget refusal, unload/reload, restart, legacy recovery, ambiguous enrollment, wrong actor refusal and no inference/replay');
}
void main().catch(error=>{console.error(error);process.exitCode=1;}).finally(async()=>{
  await owner?.close();
  await new Promise<void>(resolve=>server.close(()=>resolve()));
  fs.rmSync(directory,{recursive:true,force:true});
});
