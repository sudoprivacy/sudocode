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
const {connectCodexAutojoin,enrollCodexAgent,refreshCodexProfiles,sendScodeMessage}=require(path.join(sdk,'dist/index.cjs')) as typeof import('../src/index');
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
const gatedId=randomUUID(),gate=path.join(root,'list-release');let gatedSession: string|undefined;
const calls: string[]=[];
const server=new WebSocketServer({host:'127.0.0.1',port:0});
server.on('connection',socket=>socket.on('message',bytes=>{
  const call=JSON.parse(bytes.toString());calls.push(call.method);
  if(call.id===undefined)return;
  const result=call.method==='initialize'?{serverCapabilities:{durableQueueAddOnce:1}}:call.method==='thread/loaded/list'?{data:loaded,nextCursor:null}
    :call.method==='thread/read'?{thread:{id:call.params.threadId,cwd:workspace,status:{type:'idle'},
      parentThreadId:null,threadSource:'user',ephemeral:false,canAcceptDirectInput:true}}
    :call.method==='thread/queue/addOnce'?{queuedSubmission:{id:randomUUID(),
      clientUserMessageId:call.params.clientUserMessageId,input:call.params.input}}:{};
  socket.send(JSON.stringify({id:call.id,result}));
}));
const mintRows=()=>fs.readFileSync(path.join(root,'mint.jsonl'),'utf8').trim().split('\n').map(line=>JSON.parse(line));
const claim=(): CodexAgentClaim=>({version:1,actor:'codex-'+randomUUID().replaceAll('-',''),operationId:randomUUID(),threadId:randomUUID(),workspace});
async function main(){
  await once(server,'listening');const address=server.address();assert.ok(address&&typeof address!=='string');
  const config={native:{codexArgv:[process.execPath,path.join(sdk,'test/fixtures/codex-stdio-proxy.mjs'),String(address.port),'normal'],
    socket:'fixture-native.sock',env:{...Object.fromEntries(Object.entries(process.env).filter(([key,value])=>key!=='CODEX_THREAD_ID'&&value!==undefined)),SCODE_NATIVE_ENV_SENTINEL:'owned-proxy'},
    closeTimeoutMs:600},stateDirectory:path.join(root,'bindings'),peers:[],pollIntervalMs:100000,
    resolveAgent:async(input: CodexAgentClaim)=>{
      const result=await enrollCodexAgent(grantFile,input);
      if(input.threadId===gatedId){
        const profile=JSON.parse(fs.readFileSync(result.configFile,'utf8'));
        gatedSession=profile.acpSessionId;profile.env.SCODE_DIRECT_FIXTURE_LIST_GATE=gate;
        fs.writeFileSync(result.configFile,JSON.stringify(profile));
      }
      return result;
    }};
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
    await network.connectionFor(ids[0]).request('_fixture/peer',{from:'new-network-peer',text:'fixed allowlist must refuse'});
    await network.close();
    assert.equal(calls.filter(method=>method==='thread/queue/addOnce').length,0,'default reception remains the explicit allowlist');
    network=await connectCodexAutojoin({...config,peerPolicy:'node-namespace'});
    assert.equal(network.connectionFor(ids[0]).sessionId,sid);
    assert.equal(mintRows().length,2,'unload, resume and owner restart never mint or create another ACP session');
    await assert.rejects(network.connectionFor(ids[0]).request('_fixture/peer',
      {from:'new-network-peer',text:'wrong session must refuse',wrongSession:true}));
    await network.connectionFor(ids[0]).request('_fixture/peer',{from:'new-network-peer',text:'new Node peer data'});
    const deliveryDeadline=Date.now()+3000;
    while(!calls.includes('thread/queue/addOnce')) {
      assert.ok(Date.now()<deliveryDeadline, 'durable callback did not enqueue the accepted Node message');
      await new Promise(resolve=>setTimeout(resolve,10));
    }
    await network.close();
    assert.equal(calls.filter(method=>method==='thread/queue/addOnce').length,1,'explicit Node reception admits the new peer once, with original SID filtering');
    assert.ok(!calls.some(method=>['turn/start','turn/steer','thread/queue/add'].includes(method)));
    network=await connectCodexAutojoin(config);
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
    // An unreachable Node submits nothing: the same operation runs again once it is back.
    const outageGrant=path.join(root,'outage-grant.json');
    fs.writeFileSync(outageGrant,JSON.stringify({...grant,nexusArgv:[...grant.nexusArgv,'outage']}));
    const outage=claim();fs.writeFileSync(path.join(root,'outage'),'');
    await assert.rejects(enrollCodexAgent(outageGrant,outage),error=>(error as {outcome:string}).outcome==='not_submitted');
    assert.ok(!fs.existsSync(path.join(grant.stateDirectory,outage.actor,'operation.json')),'a refusal releases its reservation');
    assert.equal(mintRows().length,before+1);
    fs.rmSync(path.join(root,'outage'));
    assert.equal((await enrollCodexAgent(outageGrant,outage)).actor,outage.actor);
    assert.equal(mintRows().length,before+2,'the retried operation issues exactly once');
    // A failure after issuance resumes the same operation from its records: the
    // receipt replays without another mint and the one ACP session is loaded again.
    const refused=path.join(root,'list-refused');
    const resumeGrant=path.join(root,'resume-grant.json');
    fs.writeFileSync(resumeGrant,JSON.stringify({...grant,scode:{...grant.scode,
      env:{...grant.scode.env,SCODE_DIRECT_FIXTURE_LIST_REFUSED:refused}}}));
    const resumed=claim();fs.writeFileSync(refused,'');
    await assert.rejects(enrollCodexAgent(resumeGrant,resumed),
      error=>(error as {outcome:string}).outcome==='resumable' && /agent_list failed: .*transport error/.test(String(error)));
    const savedSession=JSON.parse(fs.readFileSync(path.join(grant.stateDirectory,resumed.actor,'session.json'),'utf8')).sessionId;
    const sessionsCreated=()=>fs.readFileSync(journal,'utf8').trim().split('\n').map(line=>JSON.parse(line))
      .filter(row=>row.method==='session/new').length;
    const createdBefore=sessionsCreated();
    assert.equal(mintRows().length,before+3);
    fs.rmSync(refused);
    // The owner changed the scode launch policy meanwhile: the operation resumes
    // under the policy in force now, because launch policy is not part of it.
    const completed=await enrollCodexAgent(grantFile,resumed);
    assert.equal(JSON.parse(fs.readFileSync(completed.configFile,'utf8')).acpSessionId,savedSession,
      'the resumed operation keeps its one ACP session');
    assert.equal(sessionsCreated(),createdBefore,'resuming creates no second ACP session');
    assert.equal(mintRows().length,before+3,'resuming replays the receipt without another mint');
    assert.deepEqual(await enrollCodexAgent(resumeGrant,resumed),completed,'a completed operation replays its result');
    // Nothing further is issued for a completed operation, so it replays under any
    // operator record, a reservation from before operation identity included.
    assert.deepEqual(await enrollCodexAgent(lostGrant,resumed),completed,'a completed operation outlives its Node operator command');
    const reservation=path.join(grant.stateDirectory,resumed.actor,'operation.json');
    const current=JSON.parse(fs.readFileSync(reservation,'utf8'));
    fs.writeFileSync(reservation,JSON.stringify({claim:current.claim,grantSha256:'0'.repeat(64),
      nativeCanonicalWorkspace:current.nativeCanonicalWorkspace}));
    assert.deepEqual(await enrollCodexAgent(grantFile,resumed),completed,'a pre-identity completed reservation replays');
    // An operation still in flight may have been submitted by the command that
    // started it, and only that command may drive it on.
    await assert.rejects(enrollCodexAgent(grantFile,lost),/another Node operator command/);
    const lostReservation=path.join(grant.stateDirectory,lost.actor,'operation.json');
    const lostRecord=JSON.parse(fs.readFileSync(lostReservation,'utf8'));
    fs.writeFileSync(lostReservation,JSON.stringify({claim:lostRecord.claim,grantSha256:'0'.repeat(64),
      nativeCanonicalWorkspace:lostRecord.nativeCanonicalWorkspace}));
    await assert.rejects(enrollCodexAgent(lostGrant,lost),/another Node operator command/,'a pre-identity reservation in flight keeps its hash check');
    await assert.rejects(enrollCodexAgent(grantFile,{...lost,operationId:randomUUID()}),/Another operation holds this actor/);
    assert.equal(mintRows().length,before+3,'refused resumptions submit nothing');
    // The launch policy reaches completed actors: a replay republishes it, and an
    // explicit refresh republishes every enrolled actor; neither touches the
    // operation's session, workspace or records, nor issues anything.
    const profileOf=(actor: string)=>JSON.parse(fs.readFileSync(path.join(grant.stateDirectory,actor,'profile.json'),'utf8'));
    const policyGrant=path.join(root,'policy-grant.json');
    const currentExecutable=path.join(root,path.basename(process.execPath));
    fs.copyFileSync(process.execPath,currentExecutable);
    const currentPolicy={...grant.scode,scodeArgv:[currentExecutable,...grant.scode.scodeArgv.slice(1)],closeTimeoutMs:1900};
    fs.writeFileSync(policyGrant,JSON.stringify({...grant,scode:currentPolicy}));
    const recordsBefore=['operation.json','result.json','session.json'].map(name=>fs.readFileSync(path.join(grant.stateDirectory,resumed.actor,name)));
    assert.deepEqual(await enrollCodexAgent(policyGrant,resumed),completed);
    assert.equal(profileOf(resumed.actor).closeTimeoutMs,1900,'a completed replay publishes the current launch policy');
    assert.equal(profileOf(resumed.actor).scodeArgv[0],currentExecutable);
    assert.equal(profileOf(resumed.actor).acpSessionId,savedSession);
    assert.deepEqual(['operation.json','result.json','session.json'].map(name=>fs.readFileSync(path.join(grant.stateDirectory,resumed.actor,name))),recordsBefore);
    const refreshed=JSON.parse('['+execFileSync(process.execPath,[path.join(sdk,'dist/server.cjs'),'--refresh-codex-profiles','--config',grantFile],
      {encoding:'utf8'}).trim().split('\n').join(',')+']') as {actor:string;outcome:string}[];
    assert.equal(refreshed.find(row=>row.actor===resumed.actor)?.outcome,'replaced');
    assert.equal(refreshed.find(row=>row.actor===lost.actor)?.outcome,'in_flight','an operation in flight picks the policy up when it resumes');
    assert.ok(refreshed.every(row=>row.outcome!=='refused'),JSON.stringify(refreshed));
    assert.equal(profileOf(resumed.actor).closeTimeoutMs,900);
    assert.ok(refreshCodexProfiles(grantFile).every((row: {outcome:string})=>['unchanged','in_flight'].includes(row.outcome)),'a refresh is idempotent');
    const tampered=path.join(grant.stateDirectory,outage.actor,'profile.json');const untouched=fs.readFileSync(tampered);
    fs.writeFileSync(tampered,JSON.stringify({...profileOf(outage.actor),acpSessionId:randomUUID()}));
    const refusal=(refreshCodexProfiles(grantFile) as {actor:string;outcome:string;detail?:string}[]).find(row=>row.actor===outage.actor);
    assert.ok(refusal);assert.equal(refusal.outcome,'refused');assert.match(refusal.detail??'',/belongs to another operation/);
    fs.writeFileSync(tampered,untouched);
    assert.equal(mintRows().length,before+3,'refreshing issues nothing');
    // Old equal-cwd replies still refuse private contexts without the new proof.
    const badConfig={...config,stateDirectory:path.join(root,'legacy-bindings'),resolveAgent:async(input:CodexAgentClaim)=>{
      const profile=await enrollCodexAgent(grantFile,input);delete profile.workspaceBinding;return profile;
    }};
    const old=await connectCodexAutojoin(badConfig);
    try {assert.ok(old.status().every(row=>row.state==='recovery_required' && row.error?.includes('ACP workspace differs')));}
    finally {await old.close();}
    loaded.push(gatedId);const inFlight=network.refresh();
    try {
      const end=Date.now()+5000;
      while(true){
        const rows=fs.readFileSync(journal,'utf8').trim().split('\n').map(line=>JSON.parse(line));
        if(gatedSession&&rows.filter(row=>row.method==='_sudocode/a2a/agent_list'&&row.input.sessionId===gatedSession).length===2)break;
        if(Date.now()>end)throw new Error('Actual owner identity check did not reach the controlled barrier');
        await new Promise(resolve=>setTimeout(resolve,10));
      }
      assert.equal(network.status().find(row=>row.threadId===gatedId)?.state,'connecting');
      assert.throws(()=>network.connectionFor(gatedId),/authenticated Agent/,'a live ACP transport is not admitted before Node identity verification');
    }finally {fs.writeFileSync(gate,'release');await inFlight;}
    assert.equal(network.connectionFor(gatedId).sessionId,gatedSession);
    // Ready bindings bypass enrollment. Refresh their profiles through the real
    // operator CLI, then reconnect through those same bindings and verify which
    // executable actually loads each original session and authenticates its actor.
    await network.close();
    const readyBindings=loaded.map(id=>{
      const file=path.join(config.stateDirectory,id+'.json'),bytes=fs.readFileSync(file);
      const binding=JSON.parse(bytes.toString('utf8'));
      assert.equal(binding.state,'ready');
      return {file,bytes,binding,records:['operation.json','result.json','session.json'].map(name=>({
        file:path.join(grant.stateDirectory,binding.actor,name),
        bytes:fs.readFileSync(path.join(grant.stateDirectory,binding.actor,name))}))};
    });
    const currentJournal=path.join(root,'current-acp.jsonl');
    fs.writeFileSync(policyGrant,JSON.stringify({...grant,scode:{...currentPolicy,
      env:{HYDRA_DIRECT_FIXTURE_JOURNAL:currentJournal}}}));
    const mintsBeforeRefresh=mintRows().length;
    const readyRefresh=JSON.parse('['+execFileSync(process.execPath,[path.join(sdk,'dist/server.cjs'),'--refresh-codex-profiles','--config',policyGrant],
      {encoding:'utf8'}).trim().split('\n').join(',')+']') as {actor:string;outcome:string}[];
    for(const {binding} of readyBindings)assert.equal(readyRefresh.find(row=>row.actor===binding.actor)?.outcome,'replaced');
    assert.ok(!fs.existsSync(currentJournal),'refresh does not launch ACP');
    let reenrollments=0;
    network=await connectCodexAutojoin({...config,resolveAgent:async()=>{
      reenrollments++;throw new Error('Ready binding must not invoke enrollment');
    }});
    assert.equal(reenrollments,0);
    assert.equal(network.status().filter(row=>row.state==='connected').length,readyBindings.length);
    for(const {file,bytes,binding,records} of readyBindings){
      assert.equal(network.connectionFor(binding.threadId).sessionId,binding.acp.sessionId);
      const sent=await sendScodeMessage(network.connectionFor(binding.threadId),{to:'fixture-peer',summary:'refreshed original actor',message:'refreshed original binding'});
      assert.equal(JSON.parse(sent.content).routing.sender,binding.actor);
      assert.equal(profileOf(binding.actor).closeTimeoutMs,1900);
      assert.deepEqual(fs.readFileSync(file),bytes,'refresh and ready reconnect preserve the complete binding');
      for(const record of records)assert.deepEqual(fs.readFileSync(record.file),record.bytes);
    }
    const currentProtocol=fs.readFileSync(currentJournal,'utf8').trim().split('\n').map(line=>JSON.parse(line));
    assert.ok(currentProtocol.every(row=>fs.realpathSync.native(row.executable)===fs.realpathSync.native(currentExecutable)),
      'the refreshed environment and copied executable are actually used');
    assert.deepEqual(currentProtocol.filter(row=>row.method==='session/load').map(row=>row.sessionId).sort(),
      readyBindings.map(row=>row.binding.acp.sessionId).sort());
    assert.ok(!currentProtocol.some(row=>['session/new','session/prompt'].includes(row.method)));
    assert.equal(mintRows().length,mintsBeforeRefresh,'ready reconnect never mints again');
    assert.deepEqual(fs.readFileSync(projectConfig),original);
    const firstFile=path.join(config.stateDirectory,ids[0]+'.json');
    const binding=JSON.parse(fs.readFileSync(firstFile,'utf8'));binding.workspaceBinding.operationId=randomUUID();
    fs.writeFileSync(firstFile,JSON.stringify(binding));
    await assert.rejects(connectCodexAutojoin(config),/Operator workspace binding differs/);
    const protocol=fs.readFileSync(journal,'utf8').trim().split('\n').map(line=>JSON.parse(line));
    assert.ok(!protocol.some(row=>row.method==='session/prompt'),'no inference translation');
    assert.ok(!calls.some(method=>['thread/start','thread/resume'].includes(method)));
  } finally {await network.close();}
}
void main().finally(async()=>{
  for(const socket of server.clients)socket.close();await new Promise<void>(resolve=>server.close(()=>resolve()));
  // All owned children have closed. Allow finite filesystem cleanup retries for
  // transient Windows ENOTEMPTY/EPERM; exhausting them still fails the journey.
  await fs.promises.rm(root,{recursive:true,force:true,maxRetries:3,retryDelay:100});
}).then(()=>{
  console.log('Operator enrollment process integration PASS: distinct same-project actors, private ACP bindings, original SID resume, Unknown/no retry, refusal retry, post-issuance resume, operation identity over operator policy, launch policy refresh, refreshed executable and ready binding reconnect; protocol fixture only');
}).catch(error=>{console.error(error);process.exitCode=1;});
