// Real Windows handles and public MCP entry; no simulated wait functions.
const assert=require('node:assert/strict'),fs=require('node:fs'),os=require('node:os'),path=require('node:path');
const {spawn,fork,execFile}=require('node:child_process'),{once}=require('node:events'),{setTimeout:delay}=require('node:timers/promises');
const {promisify}=require('node:util');
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
async function observePackagedStart(directory){
  // Start before the packaged process, so short-lived compiler children can
  // leave original native start/stop events even if the later snapshot misses them.
  const journal=path.join(directory,'owner-start-events.jsonl');
  const child=spawn(path.join(process.env.SystemRoot,'System32','WindowsPowerShell','v1.0','powershell.exe'),
    ['-NoLogo','-NoProfile','-NonInteractive','-File',path.join(root,'test/fixtures/windows-owner-observer.ps1'),String(process.pid)],
    {windowsHide:true,stdio:['pipe','pipe','pipe']});
  let output='',stderr='',ready=false,bytes=0,dropped=0,closing;
  const closed=new Promise(resolve=>{child.once('close',(code,signal)=>resolve({code,signal}));});
  let started;
  const initialized=new Promise(resolve=>{started=resolve;});
  const record=value=>{
    const line=JSON.stringify(value)+'\n';
    if(bytes+Buffer.byteLength(line)<=65536){
      try{fs.appendFileSync(journal,line);bytes+=Buffer.byteLength(line);}
      catch(error){stderr=('Observer journal: '+String(error)).slice(-4096);}
    }else{dropped++;}
  };
  child.on('error',error=>{record({kind:'observer_spawn_error',error:String(error)});started();});
  child.stdin.on('error',error=>{record({kind:'observer_input_error',error:String(error)});});
  child.stderr.on('data',chunk=>{stderr=(stderr+String(chunk)).slice(-4096);});
  child.stdout.setEncoding('utf8');
  child.stdout.on('data',chunk=>{
    output+=String(chunk);
    if(Buffer.byteLength(output)>65536){record({kind:'observer_output_bound'});output='';return;}
    let end;
    while((end=output.indexOf('\n'))>=0){
      const line=output.slice(0,end);output=output.slice(end+1);
      try{const row=JSON.parse(line);record(row);if(row.kind==='observer_ready'){ready=true;started();}}
      catch(error){record({kind:'observer_decode_error',error:String(error)});}
    }
  });
  void closed.then(value=>{record({kind:'observer_closed',...value,stderr});started();});
  try{await deadline(initialized,5000);}catch(error){record({kind:'observer_setup_error',error:String(error)});}
  const timers=[];
  return {
    sampleStartup(){
      if(ready)for(const ms of [3000,7000])timers.push(setTimeout(()=>{
        record({kind:'sample_requested',at:new Date().toISOString(),offsetMs:ms});
        if(!child.stdin.destroyed)child.stdin.write('s');
      },ms));
    },
    close(){return closing??=(async()=>{
      for(const timer of timers)clearTimeout(timer);
      if(!child.stdin.destroyed)child.stdin.end('q');
      try{await deadline(closed,2500);}catch(error){
        record({kind:'observer_close_error',error:String(error)});
        child.kill(); // Only this observer's returned child, never a discovered process.
        try{await deadline(closed,2500);}catch(cause){record({kind:'observer_exit_unknown',error:String(cause)});}
      }
      console.error('PACKAGED_OWNER_OBSERVER_SUMMARY='+JSON.stringify({pid:child.pid,ready,bytes,dropped,stderr}));
    })();}
  };
}
async function retainPackagedFailure(directory,transport,owner,stderr,error){
  // Run only after an original assertion fails, before its existing cleanup.
  // This current process snapshot is diagnostic, never custody/exit proof.
  const reportedMonitorPids=[...stderr.matchAll(/ownerMonitor=\{"pid":(\d+)/g)].map(match=>Number(match[1]));
  const rootPids=[...new Set([process.pid,transport.pid,owner.child.pid,...reportedMonitorPids].filter(Number.isInteger))];
  const report={observedAt:new Date().toISOString(),error:String(error),rootPids,stderr,
    parentEnvironment:Object.fromEntries(['TEMP','TMP','PSModulePath','SystemRoot'].map(key=>[key,process.env[key]??null])),
    files:{}};
  for(const name of ['fixture.jsonl','audit.jsonl','owner-start-events.jsonl']){
    try{
      const file=path.join(directory,name),size=fs.statSync(file).size,fd=fs.openSync(file,'r');
      try{
        const bytes=Buffer.alloc(Math.min(size,65536));fs.readSync(fd,bytes,0,bytes.length,Math.max(0,size-bytes.length));
        report.files[name]={bytes:size,truncated:size>bytes.length,tail:bytes.toString('utf8')};
      }finally{fs.closeSync(fd);}
    }catch(cause){report.files[name]={error:String(cause)};}
  }
  try{
    const script=`$ErrorActionPreference='Stop'
$selected=[System.Collections.Generic.HashSet[uint32]]::new()
@(${rootPids.join(',')}) | ForEach-Object { [void]$selected.Add([uint32]$_) }
$rows=@(Get-CimInstance Win32_Process)
do {
  $changed=$false
  foreach($row in $rows) {
    if($selected.Contains([uint32]$row.ParentProcessId)) {
      if($selected.Add([uint32]$row.ProcessId)) { $changed=$true }
    }
  }
} while($changed)
@($rows | Where-Object { $selected.Contains([uint32]$_.ProcessId) } |
  Select-Object ProcessId,ParentProcessId,CreationDate,Name,ExecutablePath,KernelModeTime,UserModeTime) |
  ConvertTo-Json -Compress`;
    const {stdout}=await promisify(execFile)(path.join(process.env.SystemRoot,'System32','WindowsPowerShell','v1.0','powershell.exe'),
      ['-NoLogo','-NoProfile','-NonInteractive','-EncodedCommand',Buffer.from(script,'utf16le').toString('base64')],
      {windowsHide:true,timeout:5000,maxBuffer:1024*1024,encoding:'utf8'});
    report.processSnapshot=JSON.parse(stdout);
  }catch(cause){report.processSnapshotError=String(cause);}
  fs.writeFileSync(path.join(directory,'packaged-owner-failure.json'),JSON.stringify(report,null,2)+'\n');
  // The managed CI retains this command log even when the temp directory is lost.
  console.error('PACKAGED_OWNER_FAILURE_DIAGNOSTIC='+JSON.stringify(report));
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
    const observation=await observePackagedStart(directory);
    const transport=new StdioClientTransport({command:process.execPath,args:[path.join(root,'dist/server.cjs'),'--config',config],stderr:'pipe'});
    const client=new Client({name:'receiver-owner-acceptance',version:'1'});let stderr='';
    transport.stderr?.on('data',chunk=>{stderr+=String(chunk);});
    try{
      observation.sampleStartup();
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
    }catch(error){
      await observation.close();
      try{await retainPackagedFailure(directory,transport,owner,stderr,error);}
      catch(cause){console.error('Packaged owner diagnostic failed: '+String(cause));}
      throw error;
    }finally{await observation.close();await client.close();await owner.close();}

    // Full packaged MCP + real Windows compiler-bootstrap failure. A regular
    // file is not a temporary directory. This environment is child-local;
    // there is no installed policy change or replacement close function.
    // A process ending before bootstrap_complete is deliberately UNKNOWN:
    // observing that helper alone does not reconcile compiler descendants.
    const guarded=originalProcess(),failureConfig=path.join(directory,'cleanup-config.json');
    const failureLog=path.join(directory,'cleanup-fixture.jsonl'),failureAudit=path.join(directory,'cleanup-audit.jsonl');
    const failureIdentity=await captureReceiverOwner(guarded.child.pid),failureSid=sessionId+'-cleanup';
    const invalidTemp=path.join(directory,'not-a-temp-directory');fs.writeFileSync(invalidTemp,'regular file');
    fs.writeFileSync(failureConfig,JSON.stringify({version:1,
      scodeArgv:[process.execPath,path.join(root,'test/fixtures/scode-direct-a2a-agent.mjs')],
      workspace:directory,acpSessionId:failureSid,auditLog:failureAudit,closeTimeoutMs:200,
      receiverOwner:failureIdentity,env:{HYDRA_DIRECT_FIXTURE_JOURNAL:failureLog}}));
    const host=fork(path.join(root,'test/fixtures/lifecycle-mcp.cjs'),[failureConfig],{silent:true,windowsHide:true,
      env:{...process.env,TEMP:invalidTemp,TMP:invalidTemp}});
    const hostClosed=once(host,'close');let failureStderr='';host.stderr.on('data',b=>{failureStderr+=String(b);});
    // The stream transport speaks the same JSON-RPC protocol in either role.
    const stream=new StdioServerTransport(host.stdout,host.stdin);
    const probe=new Client({name:'cleanup-ownership-acceptance',version:'1'});
    let disconnected=false,originalFailure;probe.onclose=()=>{disconnected=true;};
    try{
      await probe.connect(stream,{timeout:5000});
      const readyDeadline=Date.now()+12000;
      for(;;){
        const status=await probe.callTool({name:'scode_status',arguments:{}});
        const state=JSON.parse(status.content[0].text);
        if(state.error){
          assert.equal(state.closed,true);assert.match(state.error,/bootstrap interrupted/);
          // Both failures must survive the real packaged startup path. The
          // cleanup UNKNOWN remains authoritative; it must not hide the
          // original compiler refusal (or be downgraded by that refusal).
          assert.match(state.error,/original qualification failure: Error: Receiver owner monitor closed before qualification:/);
          assert.match(state.error,/Add-Type/);
          assert.match(state.error,/ownerMonitor=/);
          assert.match(state.error,/"lastPhase":"bootstrap_started"/);
          assert.match(state.error,/"elapsedMs":\d+/);
          originalFailure=state.error;break;
        }
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
      assert.equal(failedStatus.closed,true);assert.equal(failedStatus.error,originalFailure);
      await assert.rejects(acquireReceiverOwner(failureIdentity,failureSid),/EADDRINUSE/);
      host.send('SIGINT');await delay(100);assert.equal(host.exitCode,null);
      assert.equal(JSON.parse((await probe.callTool({name:'scode_status',arguments:{}})).content[0].text).error,originalFailure);
      assert.equal(fs.existsSync(failureLog),false,'No ACP start before owner qualification, no replacement');
      assert.equal(guarded.child.exitCode,null,'Original frontend never signalled');
      console.log('PASS full MCP bootstrap refusal: real compiler startup failure, endpoint and owner lock retained, no replacement');
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
