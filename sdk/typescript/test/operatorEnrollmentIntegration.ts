import * as assert from 'node:assert/strict';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { randomUUID } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { once } from 'node:events';
import { WebSocketServer } from 'ws';
import type { CodexAgentClaim } from '../src/codexAutojoin';
const sdk=path.resolve(__dirname,'../..');
const {connectCodexAutojoin,enrollCodexAgent,sendScodeMessage}=require(path.join(sdk,'dist/index.cjs')) as typeof import('../src/index');
const root=fs.mkdtempSync(path.join(os.tmpdir(),'codex-operator-'));
const workspace=path.join(root,'project');fs.mkdirSync(workspace);
fs.mkdirSync(path.join(workspace,'.nexus/sudocode'),{recursive:true});
const projectConfig=path.join(workspace,'.nexus/sudocode/settings.json');
fs.writeFileSync(projectConfig,'{"agentName":"original-project-actor"}\n');
const original=fs.readFileSync(projectConfig);
const journal=path.join(root,'acp.jsonl');
const grantFile=path.join(root,'grant.json');
const grant={version:1,purpose:'codex-a2a',stateDirectory:path.join(root,'operator'),
  nexusArgv:[process.execPath,path.join(sdk,'test/fixtures/operatorMint.cjs'),root],
  scode:{scodeArgv:[process.execPath,path.join(sdk,'test/fixtures/scode-direct-a2a-agent.mjs'),'operator'],
    env:{HYDRA_DIRECT_FIXTURE_JOURNAL:journal},closeTimeoutMs:900}};
fs.writeFileSync(grantFile,JSON.stringify(grant));
const ids=[randomUUID(),randomUUID()];let loaded=[...ids];
const calls: string[]=[];
const server=new WebSocketServer({host:'127.0.0.1',port:0});
server.on('connection',socket=>socket.on('message',bytes=>{
  const call=JSON.parse(bytes.toString());calls.push(call.method);
  if(call.id===undefined)return;
  const result=call.method==='initialize'?{}:call.method==='thread/loaded/list'?{data:loaded,nextCursor:null}
    :call.method==='thread/read'?{thread:{id:call.params.threadId,cwd:workspace,status:{type:'idle'},
      parentThreadId:null,threadSource:'user',ephemeral:false,canAcceptDirectInput:true}}:{};
  socket.send(JSON.stringify({id:call.id,result}));
}));
const mintRows=()=>fs.readFileSync(path.join(root,'mint.jsonl'),'utf8').trim().split('\n').map(line=>JSON.parse(line));
const claim=(): CodexAgentClaim=>({version:1,actor:'codex-'+randomUUID().replaceAll('-',''),operationId:randomUUID(),threadId:randomUUID(),workspace});
async function main(){
  await once(server,'listening');const address=server.address();assert.ok(address&&typeof address!=='string');
  const config={native:{codexArgv:[process.execPath,path.join(sdk,'test/fixtures/codex-stdio-proxy.mjs'),String(address.port),'normal'],
    socket:'fixture-native.sock',env:{...Object.fromEntries(Object.entries(process.env).filter(([key,value])=>key!=='CODEX_THREAD_ID'&&value!==undefined)),SCODE_NATIVE_ENV_SENTINEL:'owned-proxy'},
    closeTimeoutMs:600},stateDirectory:path.join(root,'bindings'),peers:[],pollIntervalMs:100000,
    resolveAgent:(input: CodexAgentClaim)=>enrollCodexAgent(grantFile,input)};
  let network=await connectCodexAutojoin(config);
  try {
    assert.equal(network.status().filter(row=>row.state==='connected').length,2,JSON.stringify(network.status()));
    assert.equal(new Set(network.status().map(row=>row.actor)).size,2,'same project has distinct actual actors');
    const sid=network.connectionFor(ids[0]).sessionId;
    for(const row of network.status()){
      const sent=await sendScodeMessage(network.connectionFor(row.threadId),{to:'fixture-peer',message:'exact 🧪\0',summary:'private actor'});
      assert.equal(JSON.parse(sent.content).routing.sender,row.actor);
      const binding=JSON.parse(fs.readFileSync(path.join(config.stateDirectory,row.threadId+'.json'),'utf8'));
      assert.notEqual(binding.acp.canonicalWorkspace,binding.canonicalWorkspace);
      assert.equal(binding.workspaceBinding.operationId,binding.operationId);
    }
    loaded=[];await network.refresh();loaded=[...ids];await network.refresh();
    assert.equal(network.connectionFor(ids[0]).sessionId,sid);
    await network.close();network=await connectCodexAutojoin(config);
    assert.equal(network.connectionFor(ids[0]).sessionId,sid);
    assert.equal(mintRows().length,2,'unload, resume and owner restart never mint or create another ACP session');
    assert.deepEqual(fs.readFileSync(projectConfig),original);
    const replay=mintRows()[0];const ready=await enrollCodexAgent(grantFile,{...replay});
    assert.equal(ready.actor,replay.actor);assert.equal(mintRows().length,2);
    // Exercise the actual public operator entry, not only its exported function.
    const publicClaim=claim();
    const result=JSON.parse(execFileSync(process.execPath,[path.join(sdk,'dist/server.cjs'),'--enroll-codex','--config',grantFile],
      {input:JSON.stringify(publicClaim),encoding:'utf8'}));
    assert.equal(result.workspaceBinding.threadId,publicClaim.threadId);
    const before=mintRows().length;
    const lostGrant=path.join(root,'lost-grant.json');
    fs.writeFileSync(lostGrant,JSON.stringify({...grant,nexusArgv:[...grant.nexusArgv,'lost']}));
    const lost=claim();await assert.rejects(enrollCodexAgent(lostGrant,lost));
    await assert.rejects(enrollCodexAgent(lostGrant,lost));assert.equal(mintRows().length,before+1,'unknown operation never resubmits');
    const invalid=claim();
    await assert.rejects(enrollCodexAgent(path.join(root,'absent-grant.json'),invalid),error=>(error as {outcome:string}).outcome==='not_submitted');
    assert.equal(mintRows().length,before+1);
    // Old equal-cwd replies still refuse private contexts without the new proof.
    const badConfig={...config,stateDirectory:path.join(root,'legacy-bindings'),resolveAgent:async(input:CodexAgentClaim)=>{
      const profile=await enrollCodexAgent(grantFile,input);delete profile.workspaceBinding;return profile;
    }};
    const old=await connectCodexAutojoin(badConfig);
    try {assert.ok(old.status().every(row=>row.state==='recovery_required' && row.error?.includes('ACP workspace differs')));}
    finally {await old.close();}
    const firstFile=path.join(config.stateDirectory,ids[0]+'.json');
    const binding=JSON.parse(fs.readFileSync(firstFile,'utf8'));binding.workspaceBinding.operationId=randomUUID();
    fs.writeFileSync(firstFile,JSON.stringify(binding));
    await assert.rejects(connectCodexAutojoin(config),/Operator workspace binding differs/);
    const protocol=fs.readFileSync(journal,'utf8').trim().split('\n').map(line=>JSON.parse(line));
    assert.ok(!protocol.some(row=>row.method==='session/prompt'),'no inference translation');
    assert.ok(!calls.some(method=>['thread/start','thread/resume','turn/start'].includes(method)));
    console.log('Operator enrollment process integration PASS: distinct same-project actors, private ACP bindings, original SID resume, Unknown/no retry; protocol fixture only');
  } finally {await network.close();}
}
void main().catch(error=>{console.error(error);process.exitCode=1;}).finally(async()=>{
  for(const socket of server.clients)socket.close();await new Promise<void>(resolve=>server.close(()=>resolve()));
  fs.rmSync(root,{recursive:true,force:true});
});
