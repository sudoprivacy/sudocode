// Isolated actual-native acceptance. Node builtins plus the existing Python TUI driver.
const fs=require('node:fs'),os=require('node:os'),path=require('node:path'),http=require('node:http');
const crypto=require('node:crypto'),assert=require('node:assert/strict');
const {spawn,execFileSync}=require('node:child_process');
const {once}=require('node:events'),{parseArgs}=require('node:util');
const {socketEntryPresent}=require('./fixtures/nativeSocket.cjs');
const options=parseArgs({options:{codex:{type:'string'},output:{type:'string'},driver:{type:'string'},
  library:{type:'string'},case:{type:'string',default:'idle'},help:{type:'boolean'}}}).values;
if(options.help){console.log('node test/nativeCodexQueue.cjs --codex ABS_NATIVE --output ABS_EVIDENCE [--case idle|response-loss] [--driver JSON_ARGV] [--library ABS_INDEX_CJS]');process.exit(0);}
assert(options.codex&&path.isAbsolute(options.codex)&&fs.statSync(options.codex).isFile());
assert(options.output&&path.isAbsolute(options.output));assert(['idle','response-loss'].includes(options.case));
const library=options.library??path.resolve(__dirname,'../dist/index.cjs');
const manifest=JSON.parse(fs.readFileSync(path.resolve(path.dirname(library),'../manifest.json')));
const hash=file=>crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
assert.equal(manifest.dirty,false,'Use a clean committed source-bound runtime');
assert.equal(hash(library),manifest.files['dist/index.cjs'].sha256);
const {connectCodexCallback}=require(library);
const driver=options.driver?JSON.parse(options.driver):process.platform==='win32'?null:['python3',path.join(__dirname,'fixtures/nativeCodexTui.py')];
assert(Array.isArray(driver)&&driver.length&&driver.every(s=>typeof s==='string'&&s&&!s.includes('\0')),'Windows requires explicit --driver JSON argv');
fs.mkdirSync(options.output,{recursive:true,mode:0o700});
const root=fs.mkdtempSync(path.join(os.tmpdir(),'codex-native-queue-'));fs.chmodSync(root,0o700);
for(const name of ['home','user','work'])fs.mkdirSync(path.join(root,name),{mode:0o700});
const home=path.join(root,'home'),workspace=path.join(root,'work'),trace=path.join(root,'tui.trace.json');
const callbackLog=path.join(root,'callback.jsonl'),relayLog=path.join(root,'relay.jsonl');
const env={};
for(const name of ['PATH','SystemRoot','WINDIR','COMSPEC','PATHEXT','TMPDIR','TEMP','TMP','LANG','LC_ALL','SHELL']){
  if(process.env[name]!==undefined)env[name]=process.env[name];
}
Object.assign(env,{CODEX_HOME:home,HOME:path.join(root,'user'),USERPROFILE:path.join(root,'user'),
  XDG_CONFIG_HOME:path.join(root,'xdg'),APPDATA:path.join(root,'roaming'),LOCALAPPDATA:path.join(root,'local'),
  TERM:'xterm-256color',NO_PROXY:'127.0.0.1,localhost,::1',no_proxy:'127.0.0.1,localhost,::1',
  NPM_CONFIG_PREFIX:path.join(root,'npm-prefix'),
  NATIVE_QUEUE_FIXTURE_KEY:'isolated-fixture-only',NATIVE_LIFECYCLE_PTY_TRACE:trace,
  NATIVE_QUEUE_RELAY_JOURNAL:relayLog});
const nonce='NATIVE_QUEUE_'+crypto.randomUUID();
const receipt={at:new Date().toISOString(),scenario:options.case,source:manifest.sourceCommit,librarySha256:hash(library),
  fixtureSha256:hash(__filename),nativeVersion:execFileSync(options.codex,['--version'],{env,encoding:'utf8'}).trim(),
  nativeSha256:hash(options.codex),driverFiles:driver.filter(s=>fs.existsSync(s)&&fs.statSync(s).isFile()).map(file=>({file,sha256:hash(file)})),
  relaySha256:hash(path.join(__dirname,'fixtures/nativeQueueResponseLoss.cjs')),
  isolation:{root,privateNativeHome:true,nativePackageManagerPrefixIsolated:true,liveHomeUsed:false,productionChanged:false},
  modelRequests:[],tuiEvents:[],producerObservations:[],notProven:['actual Windows behavior unless run there','Node/A2A authentication and receipt',
    'namespaced toolOutput callback (this is queued UserInput)','production activation']};
const wait=async(test,ms,label)=>{const end=Date.now()+ms;while(!test()){if(Date.now()>end)throw new Error(label+' timeout');await new Promise(r=>setTimeout(r,50));}};
const jsonLines=file=>fs.existsSync(file)?fs.readFileSync(file,'utf8').trim().split('\n').filter(Boolean).map(s=>JSON.parse(s)):[];
const screen=()=>{try{return JSON.parse(fs.readFileSync(trace)).outputTail??'';}catch{return '';}};
function sessionItems(directory=path.join(home,'sessions')){
  if(!fs.existsSync(directory))return [];
  return fs.readdirSync(directory,{withFileTypes:true}).flatMap(entry=>{
    const file=path.join(directory,entry.name);
    if(entry.isDirectory())return sessionItems(file);
    if(!entry.name.endsWith('.jsonl'))return [];
    // An in-flight final partial line is not yet a committed native item.
    const text=fs.readFileSync(file,'utf8'),lines=text.split('\n');if(!text.endsWith('\n'))lines.pop();
    return lines.filter(Boolean).map((line,i)=>({file,line:i+1,sha256:crypto.createHash('sha256').update(line).digest('hex'),item:JSON.parse(line)}));
  });
}
function turnItems(){return sessionItems().filter(row=>row.item.type==='event_msg'
  && ['task_started','task_complete'].includes(row.item.payload?.type));}
function ownerPid(port,remotePort){
  if(process.platform==='win32'){
    const command=`@(Get-NetTCPConnection -LocalPort ${remotePort} -RemotePort ${port} -State Established -ErrorAction Stop | Select-Object -ExpandProperty OwningProcess) | ConvertTo-Json -Compress`;
    const value=JSON.parse(execFileSync('powershell.exe',['-NoProfile','-NonInteractive','-Command',command],{env,encoding:'utf8',timeout:10000}));
    const pids=[...new Set(Array.isArray(value)?value:[value])];assert.equal(pids.length,1);return pids[0];
  }
  const lines=execFileSync(process.platform==='darwin'?'/usr/sbin/lsof':'lsof',
    ['-nP','-iTCP:'+port,'-sTCP:ESTABLISHED','-Fpn'],{env,encoding:'utf8',timeout:10000}).split('\n');
  let pid;
  for(const line of lines){if(line.startsWith('p'))pid=Number(line.slice(1));if(line===`n127.0.0.1:${remotePort}->127.0.0.1:${port}`)return pid;}
  throw new Error('Could not bind model connection to its actual process');
}
let phase='seed',tui,tuiExit,callback;
const priorOwner=process.env.CODEX_THREAD_ID;
const endpoint=http.createServer(async(req,res)=>{
  try{
    let body='';for await(const bytes of req)body+=bytes;
    const phaseAtArrival=phase,parsed=JSON.parse(body);
    receipt.modelRequests.push({at:new Date().toISOString(),phase:phaseAtArrival,originPid:ownerPid(endpoint.address().port,req.socket.remotePort),
      method:req.method,url:req.url,containsNonce:body.includes(nonce),bodySha256:crypto.createHash('sha256').update(body).digest('hex'),
      requestKind:{keys:Object.keys(parsed),textFormatType:parsed.text?.format?.type,textFormatName:parsed.text?.format?.name}});
    res.writeHead(200,{'content-type':'text/event-stream'});
    const text=phaseAtArrival==='seed'?'Native queue seed complete.':'Native queued peer response complete.';
    const responseId='resp_'+crypto.randomUUID();
    for(const event of [{type:'response.created',response:{id:responseId}},
      {type:'response.output_item.done',item:{type:'message',role:'assistant',id:'msg_'+crypto.randomUUID(),content:[{type:'output_text',text}]}},
      {type:'response.completed',response:{id:responseId,usage:{input_tokens:0,input_tokens_details:null,output_tokens:0,output_tokens_details:null,total_tokens:0}}}]){
      res.write('event: '+event.type+'\ndata: '+JSON.stringify(event)+'\n\n');
    }
    res.end();
  }catch(error){receipt.endpointError=String(error);res.destroy(error);}
});
void(async()=>{
  try{
    endpoint.listen(0,'127.0.0.1');await once(endpoint,'listening');
    fs.writeFileSync(path.join(home,'config.toml'),'check_for_update_on_startup=false\nmodel="gpt-5.4"\nmodel_provider="fixture"\napproval_policy="never"\nsandbox_mode="read-only"\n'+
      '[analytics]\nenabled=false\n[features]\napps=false\nplugins=false\n[model_providers.fixture]\nname="Isolated native queue"\n'+
      'base_url="http://127.0.0.1:'+endpoint.address().port+'/v1"\nwire_api="responses"\nenv_key="NATIVE_QUEUE_FIXTURE_KEY"\n'+
      '[projects.'+JSON.stringify(workspace)+']\ntrust_level="trusted"\n',{mode:0o600});
    tui=spawn(driver[0],[...driver.slice(1),options.codex,'--no-daemon','--no-alt-screen','--cd',workspace,'Seed isolated native queue.'],
      {cwd:workspace,env,stdio:['pipe','pipe','pipe']});tuiExit=once(tui,'close');
    let buffer='';tui.stdout.on('data',bytes=>{buffer+=bytes;let end;while((end=buffer.indexOf('\n'))>=0){
      const line=buffer.slice(0,end);buffer=buffer.slice(end+1);if(line)receipt.tuiEvents.push(JSON.parse(line));}});
    tui.stderr.on('data',bytes=>{receipt.driverStderr=((receipt.driverStderr??'')+bytes).slice(-4000);});
    await wait(()=>screen().includes('Native queue seed complete.'),30000,'seed UI response');
    await wait(()=>Date.now()-Date.parse(receipt.modelRequests.at(-1).at)>1000,10000,'seed requests settle');
    const metadata=sessionItems().filter(row=>row.item.type==='session_meta');assert.equal(metadata.length,1);
    const threadId=metadata[0].item.payload.id,originalPid=receipt.tuiEvents.find(row=>row.event==='spawn').pid;
    receipt.original={threadId,pid:originalPid,metadata:metadata[0]};
    assert(receipt.modelRequests.every(row=>row.originPid===originalPid));
    const seedTurns=turnItems();assert.equal(seedTurns.filter(row=>row.item.payload.type==='task_started').length,1);
    assert.equal(seedTurns.filter(row=>row.item.payload.type==='task_complete').length,1);
    const socket=path.join(home,'app-server-control','app-server-control.sock');assert.equal(socketEntryPresent(socket),false);
    receipt.controlSocketBefore=false;receipt.seedHttpRequests=receipt.modelRequests.length;
    // This fixture owns only its fresh thread; preserve the fixed-owner guard.
    process.env.CODEX_THREAD_ID=threadId;
    const argv=options.case==='response-loss'?[process.execPath,path.join(__dirname,'fixtures/nativeQueueResponseLoss.cjs'),options.codex]:[options.codex];
    callback=await connectCodexCallback({codexArgv:argv,nativeHome:home,env,transport:'native-queue',threadId,
      peers:['fixture-peer'],log:callbackLog,onProducerObservation:row=>receipt.producerObservations.push(row)},'existing-fixture-acp-session');
    const notification={sessionId:'existing-fixture-acp-session',update:{sessionUpdate:'user_message_chunk',content:{type:'text',text:nonce+' — exact peer 🌎\nnext line'}},
      _meta:{sudocode:{a2a:{from:'fixture-peer',kind:'message'}}}};
    callback.receive({...notification,sessionId:'wrong-session'});
    callback.receive({...notification,_meta:{sudocode:{a2a:{from:'denied-peer'}}}});
    assert.equal(receipt.producerObservations.length,0,'refused peers/SID cannot spawn a producer');
    phase='peer';const submittedAt=Date.now();callback.receive(notification);
    await wait(()=>jsonLines(callbackLog).some(row=>row.phase===(options.case==='response-loss'?'pending':'queued')),20000,'callback outcome');
    receipt.callbackAudit=jsonLines(callbackLog);
    const received=receipt.callbackAudit.filter(row=>row.phase==='received');assert.equal(received.length,1);
    receipt.eventId=received[0].eventId;
    const calls=receipt.producerObservations.filter(row=>row.phase==='request');
    assert.equal(calls.filter(row=>row.method==='thread/queue/add').length,1);
    assert(!calls.some(row=>['thread/start','thread/resume','thread/fork','turn/start','turn/steer'].includes(row.method)));
    const inventories=receipt.producerObservations.filter(row=>row.phase==='inventory');
    if(options.case==='idle'){
      assert.deepEqual(inventories.map(row=>row.loaded),[[],[]]);
      const queued=receipt.callbackAudit.find(row=>row.phase==='queued');assert.equal(queued.outcome,'queued');
      assert.equal(queued.clientUserMessageId,receipt.eventId);assert.deepEqual(queued.producerExit,{code:0,signal:null});
    }else{
      assert.equal(receipt.callbackAudit.find(row=>row.phase==='pending').outcome,'unknown');
      receipt.relayAudit=jsonLines(relayLog);
      assert.equal(receipt.relayAudit.filter(row=>row.phase==='committed-receipt-dropped').length,1);
      assert.deepEqual(receipt.relayAudit.find(row=>row.phase==='after-inventory').result,{data:[],nextCursor:null});
      const exit=receipt.relayAudit.find(row=>row.phase==='exit');assert.equal(exit.code,0);assert.equal(exit.signal,null);
      assert.equal(inventories.length,1);assert.deepEqual(inventories[0].loaded,[]);
      assert(!receipt.relayAudit.some(row=>row.phase==='unexpected-thread-loaded'));
    }
    await wait(()=>screen().includes('Native queued peer response complete.'),35000,'native queue wake');
    await wait(()=>turnItems().filter(row=>row.item.payload.type==='task_complete').length===2,10000,'queued native completion');
    receipt.wakeElapsedMs=Date.now()-submittedAt;
    const nativeTurns=turnItems();assert.equal(nativeTurns.filter(row=>row.item.payload.type==='task_started').length,2);
    receipt.nativeTurns=nativeTurns;
    const queuedItems=sessionItems().filter(row=>row.item.type==='response_item'&&row.item.payload?.role==='user'
      &&JSON.stringify(row.item.payload).includes(nonce));assert.equal(queuedItems.length,1);receipt.queuedNativeItem=queuedItems[0];
    const actualEnvelope=JSON.stringify(queuedItems[0].item.payload);
    for(const token of [receipt.eventId,'owned-scode-acp','existing-fixture-acp-session','fixture-peer','not user authority'])assert(actualEnvelope.includes(token));
    assert(receipt.modelRequests.every(row=>row.originPid===originalPid),'all inference belongs to the original target');
    assert(receipt.modelRequests.some(row=>row.containsNonce));
    receipt.producerInferenceRequests=0;
    assert.doesNotThrow(()=>process.kill(originalPid,0));assert.equal(socketEntryPresent(socket),false);
    assert.equal(sessionItems().filter(row=>row.item.type==='session_meta').length,1);
    if(options.case==='response-loss'){
      // Observe another native scheduler interval with the callback still open.
      await new Promise(resolve=>setTimeout(resolve,11000));
      assert.equal(receipt.producerObservations.filter(row=>row.phase==='request'&&row.method==='thread/queue/add').length,1);
      assert.equal(turnItems().filter(row=>row.item.payload.type==='task_started').length,2);
      receipt.noReplayObservationMs=11000;
    }
    assert.equal(receipt.endpointError,undefined,'Every model request must have a verified process origin');
    receipt.result='PASS';
  }catch(error){receipt.result='FAIL';receipt.error={message:error.message,stack:error.stack};process.exitCode=1;}
  finally{
    phase='cleanup';if(callback)await callback.close().catch(error=>{receipt.result='FAIL';receipt.callbackCloseError=String(error);process.exitCode=1;});
    if(tui){tui.stdin.end(JSON.stringify({action:'close'})+'\n');
      try{await Promise.race([tuiExit,new Promise((_,reject)=>{const timer=setTimeout(()=>reject(new Error('TUI exit timeout')),12000);timer.unref();})]);}
      catch(error){receipt.result='FAIL';receipt.closeError=String(error);process.exitCode=1;tui.kill('SIGTERM');}
    }
    const exit=receipt.tuiEvents.find(row=>row.event==='exit');receipt.normalTuiExit=exit?.returncode===0
      &&!receipt.tuiEvents.some(row=>['terminate','kill','normal_exit_timeout'].includes(row.event));
    if(!receipt.normalTuiExit){receipt.result='FAIL';process.exitCode=1;}
    if(priorOwner===undefined)delete process.env.CODEX_THREAD_ID;else process.env.CODEX_THREAD_ID=priorOwner;
    await new Promise(resolve=>endpoint.close(resolve));
    if(receipt.endpointError){receipt.result='FAIL';process.exitCode=1;}
    const file=path.join(options.output,'native-codex-queue-'+options.case+'-'+receipt.at.replace(/[:.]/g,'')+'.receipt.json');
    fs.writeFileSync(file,JSON.stringify(receipt,null,2)+'\n',{flag:'wx',mode:0o600});
    console.log(JSON.stringify({result:receipt.result,error:receipt.error,receipt:file,original:receipt.original&&{pid:receipt.original.pid,threadId:receipt.original.threadId},
      producerInferenceRequests:receipt.producerInferenceRequests,modelHttpRequests:receipt.modelRequests.length,normalTuiExit:receipt.normalTuiExit}));
  }
})();
