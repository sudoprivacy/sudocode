// Actual operator client + legacy live mTLS Node + real scode ACP. The common
// nativeFixture owns/cleans only its private Node; it never touches the fleet.
const fs=require('node:fs'),path=require('node:path'),http=require('node:http');
const assert=require('node:assert/strict');
const {createHash,randomUUID}=require('node:crypto');
const {execFileSync}=require('node:child_process');
const config=JSON.parse(fs.readFileSync(process.argv[2],'utf8'));
const sdk=require(path.join(config.install,'node_modules/@sudocode/acp/dist/index.cjs'));
const entry=path.join(config.install,'node_modules/@sudocode/acp/dist/server.cjs');
const digest=file=>createHash('sha256').update(fs.readFileSync(file)).digest('hex');
assert.equal(digest(config.operator),config.operatorSha256);assert.equal(digest(config.scode),config.scodeSha256);
const receipts=[];const connections=[];let requests=0;let model;
const wire=[];
const run=(exe,args,input)=>JSON.parse(execFileSync(exe,args,{input,encoding:'utf8',stdio:['pipe','pipe','pipe']}));
const claim=()=>({version:1,actor:'codex-'+randomUUID().replaceAll('-',''),operationId:randomUUID(),threadId:randomUUID(),workspace:config.actors[0].workspace});
const claimA=claim(),claimB=claim();
async function main(){
  const verified=run(process.execPath,[entry,'--verify']);assert.equal(verified.sourceCommit,config.packageSource);
  model=http.createServer((req,res)=>{if(req.method!=='GET')requests++;res.writeHead(req.method==='GET'?404:503);res.end('{}');});
  await new Promise(resolve=>model.listen(0,'127.0.0.1',resolve));
  const url=`http://127.0.0.1:${model.address().port}`;
  const sample=fs.readFileSync(config.sampleConfig,'utf8').replaceAll('https://api.anthropic.com',url).replaceAll('<YOUR_ANTHROPIC_API_KEY>','isolated-operator-no-inference');
  fs.writeFileSync(path.join(config.actors[0].configHome,'sudocode.json'),sample);
  const nodeGrant=path.join(config.root,'operator-node-grant.json');
  const initialized=run(config.operator,['--data-dir',config.dataDirectory,'operator-agent-enroll','--initialize','--grant',nodeGrant,
    '--endpoint',config.endpoint,'--workspace-root',claimA.workspace]);
  assert.equal(initialized.issued,false);
  const grant=path.join(config.root,'operator-acp-grant.json');
  const policy={scodeArgv:[config.scode,'--auth','api-key','--model','sonnet','--permission-mode','workspace-write',
    '--allowedTools','agent_list','--allowedTools','send','acp'],env:config.actors[0].env,closeTimeoutMs:3000};
  const prepared=sdk.prepareCodexEnrollment(grant,[config.operator,'operator-agent-enroll','--grant',nodeGrant],policy,entry);
  assert.equal(prepared.issued,false);
  const original=fs.readFileSync(path.join(claimA.workspace,'.nexus/sudocode/settings.json'));
  const enroll=claim=>run(process.execPath,[entry,'--enroll-codex','--config',grant],JSON.stringify(claim));
  const a=enroll(claimA),b=enroll(claimB);
  for(const [claim,reply] of [[claimA,a],[claimB,b]]){
    assert.equal(reply.actor,claim.actor,JSON.stringify(reply));assert.equal(reply.workspaceBinding.operationId,claim.operationId);
    const profile=JSON.parse(fs.readFileSync(reply.configFile));assert.notEqual(fs.realpathSync(profile.workspace),fs.realpathSync(claim.workspace));
    assert.ok(profile.acpSessionId);assert.ok(!JSON.stringify(profile).includes('node-key.pem'));
    receipts.push({actor:claim.actor,operationId:claim.operationId,sessionId:profile.acpSessionId,profileSha256:digest(reply.configFile)});
  }
  assert.deepEqual(fs.readFileSync(path.join(claimA.workspace,'.nexus/sudocode/settings.json')),original,'original project identity unchanged');
  assert.deepEqual(enroll(claimA),a,'exact completed profile receipt replay');
  assert.equal(requests,0,'enrollment/list/create/close do not infer');
  const profileA=JSON.parse(fs.readFileSync(a.configFile)),profileB=JSON.parse(fs.readFileSync(b.configFile));
  let notices=[];
  const receiver=await sdk.connectScode(profileB,n=>notices.push(n));connections.push(receiver);
  const sender=await sdk.connectScode({...profileA,onProtocol:(direction,message)=>wire.push({direction,message})});connections.push(sender);
  assert.equal(sender.sessionId,profileA.acpSessionId);assert.equal(receiver.sessionId,profileB.acpSessionId);
  const listed=await sdk.listScodeAgents(sender);assert.equal(listed.isError,false,JSON.stringify(listed));
  assert.ok(listed.content.includes(claimB.actor));
  const body='OPERATOR_REAL_NODE '+config.marker+' 🧪\0';
  const sent=await sdk.sendScodeMessage(sender,{to:claimB.actor,message:body,summary:'Isolated operator enrollment acceptance'});
  assert.equal(sent.isError,false,JSON.stringify(sent));assert.equal(sdk.scodeNativeA2aAudit(sent).actor,claimA.actor);
  const deadline=Date.now()+30000;
  while(!notices.some(n=>n.update?.content?.text?.includes(body))){
    if(Date.now()>deadline)throw new Error('Real Node recipient did not receive the exact enrollment test message');
    await new Promise(resolve=>setTimeout(resolve,50));
  }
  assert.equal(notices.filter(n=>n.update?.content?.text?.includes(body)).length,1);
  assert.ok(!wire.some(row=>row.direction==='outgoing'&&row.message?.method==='session/prompt'));
  const receipt={success:true,realLegacyTlsNode:true,operatorSource:config.operatorSource,operatorSha256:config.operatorSha256,
    scodeSource:config.source,scodeSha256:config.scodeSha256,packageSource:config.packageSource,daemon:config.daemonReceipt,
    privateSameProjectActors:true,originalProjectUnchanged:true,originalAcpSidsReloaded:true,enrollmentInferenceRequests:0,
    identities:receipts,listed,sent,limits:['not global installation','not native Codex new/resume composition','not lifecycle/HA or production credential acceptance']};
  fs.writeFileSync(path.join(config.root,'native-enrollment.receipt.json'),JSON.stringify(receipt,null,2)+'\n');
  console.log(JSON.stringify({success:true,realLegacyTlsNode:true,privateSameProjectActors:true,originalAcpSidsReloaded:true,enrollmentInferenceRequests:0}));
}
main().catch(error=>{console.error(error);process.exitCode=1;}).finally(async()=>{
  await Promise.allSettled(connections.map(connection=>connection.close()));
  if(model)await new Promise(resolve=>model.close(resolve));
  fs.writeFileSync(path.join(config.root,'native-enrollment-epochs.json'),JSON.stringify({requests,wire},null,2));
});
