// Real Windows handles and public MCP entry; no simulated wait functions.
const assert=require('node:assert/strict'),fs=require('node:fs'),os=require('node:os'),path=require('node:path');
const {spawn,fork}=require('node:child_process'),{once}=require('node:events'),{setTimeout:delay}=require('node:timers/promises');
const {Client}=require('@modelcontextprotocol/sdk/client/index.js');
const {StdioClientTransport}=require('@modelcontextprotocol/sdk/client/stdio.js');
const {StdioServerTransport}=require('@modelcontextprotocol/sdk/server/stdio.js');
const {captureReceiverOwner,acquireReceiverOwner}=require('../dist/index.cjs');
const root=path.resolve(__dirname,'..');
async function deadline(value,ms=12000){
  let timer;
  try{return await Promise.race([value,new Promise((_,reject)=>{timer=setTimeout(()=>reject(Error('Process acceptance deadline')),ms);})]);}
  finally{clearTimeout(timer);}
}
function originalProcess(){
  const child=spawn(process.execPath,['-e','process.stdin.resume();process.stdin.on("end",()=>process.exit(0));'],
    {windowsHide:true,stdio:['pipe','ignore','pipe']});
  const closed=once(child,'close');
  let closing;
  return {child,close(){return closing??=(async()=>{child.stdin.end();await deadline(closed);})();}};
}
async function main(){
  if(process.platform!=='win32'){console.log('SKIP Windows handle adapter on non-Windows; shared MCP tests remain required');return;}
  const directory=fs.mkdtempSync(path.join(os.tmpdir(),'scode-receiver-owner-'));
  const config=path.join(directory,'runtime.json');fs.writeFileSync(config,'{}');
  const sessionId=path.basename(directory);
  const original=originalProcess();let lease,completed=false;
  try{
    const identity=await captureReceiverOwner(original.child.pid);
    assert.equal(identity.pid,original.child.pid);assert.match(identity.createdFiletime100ns,/^[0-9]+$/);
    lease=await acquireReceiverOwner(identity,sessionId);
    let ended=false;void lease.ended.then(()=>{ended=true;});
    await delay(6200);assert.equal(ended,false,'Live owner survives old5000ms false-death boundary');
    await assert.rejects(acquireReceiverOwner({...identity},sessionId),/EADDRINUSE/);
    await lease.close();lease=undefined;
    assert.equal(original.child.exitCode,null,'Guard close never terminates original owner');
    await assert.rejects(acquireReceiverOwner({...identity,createdFiletime100ns:String(BigInt(identity.createdFiletime100ns)+1n)},sessionId),/identity differs/);
    lease=await acquireReceiverOwner(identity,sessionId);
    process.kill(lease.monitorPid); // Only this test's actual returned child.
    assert.equal((await deadline(lease.ended)).reason,'monitor_failed');
    assert.equal(original.child.exitCode,null);
    await lease.close();lease=undefined;
    lease=await acquireReceiverOwner(identity,sessionId);await original.close();
    assert.equal((await deadline(lease.ended)).reason,'owner_exited');
    await lease.close();lease=undefined;
    await assert.rejects(acquireReceiverOwner(identity,sessionId));
    console.log('PASS real Windows handles: idle, duplicate, stale birth, monitor failure, owner exit, EOF cleanup');

    const owner=originalProcess(),log=path.join(directory,'fixture.jsonl'),auditLog=path.join(directory,'audit.jsonl');
    fs.writeFileSync(config,JSON.stringify({version:1,scodeArgv:[process.execPath,path.join(root,'test/fixtures/scode-direct-a2a-agent.mjs')],
      workspace:directory,acpSessionId:'receiver-owner-existing-session',auditLog,closeTimeoutMs:200,
      receiverOwner:await captureReceiverOwner(owner.child.pid),env:{HYDRA_DIRECT_FIXTURE_JOURNAL:log}}));
    const transport=new StdioClientTransport({command:process.execPath,args:[path.join(root,'dist/server.cjs'),'--config',config],stderr:'pipe'});
    const client=new Client({name:'receiver-owner-acceptance',version:'1'});let stderr='';
    transport.stderr?.on('data',chunk=>{stderr+=String(chunk);});
    try{
      await client.connect(transport,{timeout:5000});
      const end=Date.now()+12000;let ready=false;
      while(Date.now()<end){
        const result=await client.callTool({name:'scode_status',arguments:{}});
        if(JSON.parse(result.content[0].text).closed===false){ready=true;break;}
        await delay(30);
      }
      assert.equal(ready,true,stderr);
      const sent=await client.callTool({name:'scode_send',arguments:{to:'fixture-peer',message:'one exact original-session call',summary:'owner lifetime integration'}});
      assert.equal(sent.isError,false,JSON.stringify(sent));
      const closed=new Promise(resolve=>{client.onclose=resolve;});
      await owner.close();await deadline(closed);
      const rows=fs.readFileSync(log,'utf8').trim().split('\n').map(JSON.parse);
      assert.equal(rows.filter(row=>row.method==='session/new').length,0);
      assert.equal(rows.filter(row=>row.method==='_sudocode/a2a/send').length,1);
      const audit=fs.readFileSync(auditLog,'utf8').trim().split('\n').map(JSON.parse);
      assert.equal(audit.filter(row=>row.phase==='receiver_owner_bound').length,1);
      assert.equal(audit.find(row=>row.phase==='receiver_owner_ended')?.reason,'owner_exited');
      console.log('PASS packaged MCP: one config/SID/guard; real owner exit closes receiver');
    }finally{await client.close();await owner.close();}

    // Full packaged MCP + actual ACP child cleanup refusal. The bounded
    // stderr holder is real, not a replacement close function or fake result.
    const guarded=originalProcess(),failureConfig=path.join(directory,'cleanup-config.json');
    const failureLog=path.join(directory,'cleanup-fixture.jsonl'),failureAudit=path.join(directory,'cleanup-audit.jsonl');
    const failureIdentity=await captureReceiverOwner(guarded.child.pid),failureSid=sessionId+'-cleanup';
    fs.writeFileSync(failureConfig,JSON.stringify({version:1,
      scodeArgv:[process.execPath,path.join(root,'test/fixtures/scode-direct-a2a-agent.mjs'),'cleanup-unknown'],
      workspace:directory,acpSessionId:failureSid,auditLog:failureAudit,closeTimeoutMs:200,
      receiverOwner:failureIdentity,env:{HYDRA_DIRECT_FIXTURE_JOURNAL:failureLog}}));
    const host=fork(path.join(root,'test/fixtures/lifecycle-mcp.cjs'),[failureConfig],{silent:true,windowsHide:true});
    const hostClosed=once(host,'close');let failureStderr='';host.stderr.on('data',b=>{failureStderr+=String(b);});
    // The stream transport speaks the same JSON-RPC protocol in either role.
    const stream=new StdioServerTransport(host.stdout,host.stdin);
    const probe=new Client({name:'cleanup-ownership-acceptance',version:'1'});
    let disconnected=false;probe.onclose=()=>{disconnected=true;};
    try{
      await probe.connect(stream,{timeout:5000});
      const readyDeadline=Date.now()+12000;
      for(;;){
        const status=await probe.callTool({name:'scode_status',arguments:{}});
        if(JSON.parse(status.content[0].text).closed===false)break;
        assert.ok(Date.now()<readyDeadline,failureStderr);await delay(30);
      }
      host.send('SIGTERM');
      const refusedDeadline=Date.now()+12000;
      while(!failureStderr.includes('Shutdown refused; cleanup UNKNOWN')){
        assert.ok(Date.now()<refusedDeadline,JSON.stringify({failureStderr,hostExit:host.exitCode,hostSignal:host.signalCode,
          fixture:fs.existsSync(failureLog)?fs.readFileSync(failureLog,'utf8'):null,
          audit:fs.existsSync(failureAudit)?fs.readFileSync(failureAudit,'utf8'):null}));await delay(30);
      }
      assert.equal(disconnected,false,'Cleanup UNKNOWN must not close MCP and invite frontend disposal');
      assert.equal(host.exitCode,null,'Graceful signal must retain the receiver process');
      const failedStatus=JSON.parse((await probe.callTool({name:'scode_status',arguments:{}})).content[0].text);
      assert.equal(failedStatus.closed,true);assert.ok(failedStatus.error);
      await assert.rejects(acquireReceiverOwner(failureIdentity,failureSid),/EADDRINUSE/);
      host.send('SIGINT');await delay(100);assert.equal(host.exitCode,null);
      const events=fs.readFileSync(failureLog,'utf8').trim().split('\n').map(JSON.parse);
      assert.equal(events.filter(r=>r.method==='session/load').length,1,'No replacement ACP process');
      const holder=events.find(r=>r.method==='cleanup-holder');assert.ok(holder);
      assert.throws(()=>process.kill(holder.pid,0),{code:'ESRCH'},'The bounded holder actually finished');
      console.log('PASS full MCP cleanup refusal: actual stderr-held child, endpoint and owner lock retained, no replacement');
    }finally{
      await probe.close();
      if(host.exitCode===null&&host.signalCode===null)host.kill('SIGKILL');
      await deadline(hostClosed);await guarded.close();
    }
    completed=true;
  }finally{
    await lease?.close();await original.close();
    assert.equal(path.dirname(directory),path.resolve(os.tmpdir()));assert.ok(path.basename(directory).startsWith('scode-receiver-owner-'));
    if(completed)fs.rmSync(directory,{recursive:true,force:true});
    else console.error('Failed process evidence retained at '+directory);
  }
}
void main().catch(error=>{console.error(error);process.exitCode=1;});
