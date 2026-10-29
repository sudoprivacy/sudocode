const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const http = require('node:http');
const crypto = require('node:crypto');
const assert = require('node:assert/strict');
const { spawn, execFileSync } = require('node:child_process');
const { once } = require('node:events');

const { parseArgs } = require('node:util');
const options = parseArgs({ options: {
  codex: {type:'string'}, output: {type:'string'}, case: {type:'string',default:'idle'},
  driver: {type:'string'}, 'temp-root': {type:'string'}, 'mcp-context': {type:'boolean'}, autojoin: {type:'boolean'}, help: {type:'boolean'},
} }).values;
if (options.help) {
  console.log('node test/nativeCodexLifecycle.cjs --codex <native executable> --output <evidence directory> [--case idle|cancelled-resume|completed-resume] [--driver <JSON argv>] [--temp-root <private directory>]');
  process.exit(0);
}
const executable = options.codex ?? process.env.CODEX_TEST_BIN;
assert(executable && path.isAbsolute(executable) && fs.statSync(executable).isFile(), 'Select an existing absolute native Codex executable, not a shell launcher');
assert(!['.cmd','.bat','.ps1'].includes(path.extname(executable).toLowerCase()), 'Use the native executable under shell:false');
assert(['idle','cancelled-resume','completed-resume'].includes(options.case), 'Unknown lifecycle case');
assert(options.output, 'An explicit evidence output directory is required');
const evidence = path.resolve(options.output);
fs.mkdirSync(evidence,{recursive:true,mode:0o700});
const library = path.resolve(__dirname,'../dist/index.cjs');
const buildManifest = JSON.parse(fs.readFileSync(path.resolve(__dirname,'../manifest.json'),'utf8'));
const librarySha256 = crypto.createHash('sha256').update(fs.readFileSync(library)).digest('hex');
assert.match(buildManifest.sourceCommit,/^[0-9a-f]{40}$/);
assert.equal(buildManifest.files['dist/index.cjs'].sha256,librarySha256,'Use the hash-bound canonical library');
const { connectCodex } = require(library);
assert.equal(typeof connectCodex,'function','Build the canonical SDK including #1080 first');
const driverArgv = options.driver ? JSON.parse(options.driver) : process.platform === 'win32' ? null
  : ['python3',path.join(__dirname,'fixtures/nativeCodexTui.py')];
assert(Array.isArray(driverArgv) && driverArgv.length && driverArgv.every(arg=>typeof arg==='string' && arg && !arg.includes('\0')),
  'A real TUI driver is required; Windows must provide --driver JSON argv. No protocol-front-end fallback.');
const root = fs.mkdtempSync(path.join(options['temp-root'] ?? os.tmpdir(), 'cx-life-'));
fs.chmodSync(root, 0o700);
const isolatedHome = path.join(root,'home');
const workspace = path.join(root,'work');
fs.mkdirSync(isolatedHome);fs.mkdirSync(workspace);
const cancelledResume = options.case==='cancelled-resume';
const seededResume = options.case!=='idle';
const selectedSocket = path.join(isolatedHome,'app-server-control/app-server-control.sock');
const configured = { codexArgv:[executable] };
const childEnv={};
for(const key of ['PATH','SystemRoot','WINDIR','COMSPEC','PATHEXT','TMPDIR','TEMP','TMP','LANG','LC_ALL','SHELL','DOTNET_ROOT','DOTNET_ROOT_X64']){
  if(process.env[key]!==undefined)childEnv[key]=process.env[key];
}
const userHome=path.join(root,'user-home');fs.mkdirSync(userHome);
const loopbackHosts='127.0.0.1,localhost,::1';
Object.assign(childEnv,{CODEX_HOME:isolatedHome,HOME:userHome,USERPROFILE:userHome,XDG_CONFIG_HOME:path.join(root,'xdg'),
  APPDATA:path.join(root,'roaming'),LOCALAPPDATA:path.join(root,'local'),
  NATIVE_LIFECYCLE_FIXTURE_KEY:'isolated-fixture-only',TERM:'xterm-256color',
  NO_PROXY:loopbackHosts,no_proxy:loopbackHosts});
const receipt = {
  at: new Date().toISOString(), scenario:options.case, platform:process.platform, driverArgv,
  probeSourceSha256:crypto.createHash('sha256').update(fs.readFileSync(__filename)).digest('hex'),
  source: buildManifest.sourceCommit, libraryBuildDirty: buildManifest.dirty,
  librarySha256,
  driverFiles:driverArgv.filter(arg=>fs.existsSync(arg) && fs.statSync(arg).isFile()).map(file=>({
    file,sha256:crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex'),
  })),
  nativeSha256: crypto.createHash('sha256').update(fs.readFileSync(configured.codexArgv[0])).digest('hex'),
  nativeVersion: execFileSync(configured.codexArgv[0], ['--version'], {encoding:'utf8',env:childEnv}).trim(),
  isolation: {root, isolatedCodexHome:true, liveEndpointUsed:false, nexusUsed:false, liveConfigChanged:false},
  requests: [], observerNotifications: [], modelRequests: [], stages: [], tuiProcesses: [],
};
const bounded = (promise, ms, label) => Promise.race([promise, new Promise((_, reject) => {
  const timer = setTimeout(() => reject(new Error(label+' timeout')), ms); timer.unref();
})]);
const waitUntil = async (test, ms, label) => {
  const end = Date.now()+ms;
  while (!test()) { if (Date.now()>end) throw new Error(label+' timeout'); await new Promise(resolve=>setTimeout(resolve,25)); }
};
let phase='idle';
const heldResponses=new Set();
const endpoint = http.createServer((req,res)=>{
  receipt.modelRequests.push({method:req.method,url:req.url,phase});
  if(phase==='cancelled-seed' && req.method==='POST' && req.url==='/v1/responses'){
    // A 503 drives retries before the user item appears on some native builds.
    // Keep this explicit cancelled seed pending until turn/interrupt or cleanup.
    heldResponses.add(res);
    res.once('close',()=>heldResponses.delete(res));
    req.resume();return;
  }
  if(phase==='completed-seed' && req.method==='POST' && req.url==='/v1/responses'){
    req.resume();res.writeHead(200,{'content-type':'text/event-stream'});
    for(const event of [
      {type:'response.created',response:{id:'resp_fixture_seed'}},
      {type:'response.output_item.done',item:{type:'message',role:'assistant',id:'msg_fixture_seed',content:[{type:'output_text',text:'Isolated lifecycle seed response.'}]}},
      {type:'response.completed',response:{id:'resp_fixture_seed',usage:{input_tokens:0,input_tokens_details:null,output_tokens:0,output_tokens_details:null,total_tokens:0}}},
    ])res.write('event: '+event.type+'\ndata: '+JSON.stringify(event)+'\n\n');
    res.end();return;
  }
  req.resume(); res.writeHead(503, {'content-type':'application/json'}); res.end(JSON.stringify({error:'Model requests forbidden in lifecycle test'}));
});
const connections = [];
const tuis = [];
let native;
let nativeExited;
let nativeStderr = '';
const request = async (client,label,method,params) => {
  receipt.requests.push({client:label,method,threadId:params.threadId});
  return client.request(method,params);
};
const connect = async (label, events) => {
  // --sock selects only this fixture endpoint; no env wrapper or second transport.
  const client=await connectCodex({codexArgv:configured.codexArgv,socket:selectedSocket,env:childEnv}, n=>{
    events.push({method:n.method,threadId:n.params?.thread?.id ?? n.params?.threadId});
  });
  connections.push(client);
  receipt.stages.push(label+' connected');
  return client;
};
const closeTui = async own => {
  if(own.closed)return;
  own.closed=true;
  own.driver.stdin.end(JSON.stringify({action:'close'})+'\n');
  try{await bounded(own.exit,10000,'isolated TUI driver close');}catch(error){
    receipt.cleanup.push(String(error));receipt.result='FAIL';process.exitCode=1;
    const pid=own.lines.find(line=>line.event==='spawn')?.pid;
    if(pid){try{process.kill(pid,'SIGKILL');}catch(e){receipt.cleanup.push({fixturePid:pid,signalError:e.code});}}
    try{own.driver.kill('SIGTERM');await bounded(own.exit,3000,'isolated driver forced exit');}
    catch(e){receipt.cleanup.push({driverExitError:String(e)});}
  }
  receipt.tuiProcesses.push({label:own.label,events:own.lines,stderr:own.stderr});
  const final=own.lines.find(line=>line.event==='exit');
  assert(final && final.returncode===0 && !own.lines.some(line=>['terminate','kill','normal_exit_timeout'].includes(line.event)),
    'Actual TUI must exit normally; forced cleanup is not lifecycle acceptance');
};
(async()=>{
  try {
    endpoint.listen(0,'127.0.0.1'); await once(endpoint,'listening');
    const apiPort=endpoint.address().port;
    let autojoinConfig;
    if(options.autojoin){
      const listener=require('node:net').createServer();listener.listen(0,'127.0.0.1');await once(listener,'listening');
      const port=listener.address().port;await new Promise(resolve=>listener.close(resolve));
      autojoinConfig=path.join(root,'autojoin.json');
      fs.writeFileSync(autojoinConfig,JSON.stringify({version:1,mode:'codex-autojoin',
        native:{codexArgv:configured.codexArgv,socket:selectedSocket,env:childEnv},
        stateDirectory:path.join(root,'bindings'),auditLog:path.join(root,'autojoin-audit.jsonl'),peers:[],
        enrollmentArgv:[process.execPath,path.join(__dirname,'fixtures/codexAgentEnrollment.cjs'),root],
        endpoint:{port,token:crypto.randomBytes(32).toString('hex')},
      }),{mode:0o600});
    }
    fs.writeFileSync(path.join(isolatedHome,'config.toml'),
      'model = "gpt-5.4"\nmodel_provider = "lifecycle_fixture"\napproval_policy = "never"\nsandbox_mode = "read-only"\n'+
      '[features]\napps = false\nplugins = false\n[analytics]\nenabled = false\n[model_providers.lifecycle_fixture]\nname = "No inference lifecycle fixture"\n'+
      'base_url = "http://127.0.0.1:'+apiPort+'/v1"\nwire_api = "responses"\nenv_key = "NATIVE_LIFECYCLE_FIXTURE_KEY"\n'+
      '[projects.'+JSON.stringify(workspace)+']\ntrust_level = "trusted"\n'+
      (options['mcp-context'] ? '[mcp_servers.context_probe]\ncommand = '+JSON.stringify(process.execPath)+
        '\nargs = ['+JSON.stringify(path.join(__dirname,'fixtures/mcpStartupContext.cjs'))+']\nrequired = true\n'+
        '[mcp_servers.context_probe.env]\nCODEX_MCP_CONTEXT_OUTPUT = '+JSON.stringify(root)+'\n' : '')+
      (options.autojoin ? '[mcp_servers.nexus_auto]\ncommand = '+JSON.stringify(process.execPath)+
        '\nargs = ['+[path.resolve(__dirname,'../dist/server.cjs'),'--codex','--config',autojoinConfig].map(s=>JSON.stringify(s)).join(',')+']\nrequired = true\n' : ''), {mode:0o600});
    native=spawn(configured.codexArgv[0],[...configured.codexArgv.slice(1),'app-server','--listen','unix://'],
      {cwd:workspace,env:childEnv,stdio:['ignore','ignore','pipe']});
    nativeExited=once(native,'close');
    native.stderr.on('data',data=>{nativeStderr=(nativeStderr+data).slice(-16000);});
    await waitUntil(()=>{
      // Windows AF_UNIX entries cannot be stat'ed; listing does not open them.
      // The actual canonical connection below still verifies the listener.
      try{return fs.readdirSync(path.dirname(selectedSocket)).includes(path.basename(selectedSocket));}
      catch(error){if(error.code==='ENOENT')return false;throw error;}
    },10000,'isolated native listener');
    const observer=await connect('observer',receipt.observerNotifications);
    {
      const spawnTui = async (label,args=[]) => {
        const driver=spawn(driverArgv[0],[...driverArgv.slice(1),configured.codexArgv[0],
          ...configured.codexArgv.slice(1),...args,'--no-alt-screen','--cd',workspace],{cwd:workspace,
            env:{...childEnv,NATIVE_LIFECYCLE_PTY_TRACE:path.join(root,'tui-'+label+'.trace.json')},stdio:['pipe','pipe','pipe']});
        const own={label,driver,exit:once(driver,'close'),lines:[],stderr:''};tuis.push(own);
        let buffer='';driver.stdout.on('data',bytes=>{buffer+=bytes;let end;while((end=buffer.indexOf('\n'))>=0){const line=buffer.slice(0,end);buffer=buffer.slice(end+1);own.lines.push(JSON.parse(line));}});
        driver.stderr.on('data',bytes=>{own.stderr=(own.stderr+bytes).slice(-4000);});
        await waitUntil(()=>own.lines.some(line=>line.event==='spawn'),5000,'TUI spawn');
        return own;
      };
      for(const label of ['first','second'])await spawnTui(label);
      let inventory;
      let loadedThreads=[];
      receipt.readiness=[];
      const deadline=Date.now()+20000;
      do {
        inventory=await request(observer,'observer','thread/loaded/list',{});
        assert.equal(inventory.nextCursor,null,'The isolated inventory must be complete');
        if(inventory.data.length===2){
          loadedThreads=[];
          for(const id of inventory.data){
            const {thread}=await request(observer,'observer','thread/read',{threadId:id,includeTurns:false});
            loadedThreads.push(thread);
          }
          const states=loadedThreads.map(thread=>({id:thread.id,status:thread.status.type}));
          if(JSON.stringify(receipt.readiness.at(-1))!==JSON.stringify(states))receipt.readiness.push(states);
          if(loadedThreads.every(thread=>thread.status.type==='idle'))break;
        }
        if(Date.now()>deadline)throw new Error('Two actual idle TUIs not visible on default endpoint');
        await new Promise(resolve=>setTimeout(resolve,150));
      }while(true);
      receipt.created=[];
      for(const thread of loadedThreads){
        const id=thread.id;
        assert.equal(fs.realpathSync(thread.cwd),fs.realpathSync(workspace));assert.equal(thread.status.type,'idle');
        receipt.created.push({id,cwd:thread.cwd,status:thread.status,source:thread.source,
          hasParentField:Object.hasOwn(thread,'parentThreadId'),parentThreadId:thread.parentThreadId??null,
          threadSource:thread.threadSource,ephemeral:thread.ephemeral,
          canAcceptDirectInput:thread.canAcceptDirectInput??null});
      }
      assert.notEqual(...receipt.created.map(thread=>thread.id));
      receipt.inventory=inventory.data;
      receipt.startedSeenByObserver=receipt.observerNotifications.filter(n=>n.method==='thread/started').map(n=>n.threadId);
      assert(receipt.created.every(thread=>receipt.startedSeenByObserver.includes(thread.id)));
      assert.equal(receipt.modelRequests.length,0);
      receipt.modelRequestsBeforeSeed=0;
      if(options['mcp-context']){
        await waitUntil(()=>fs.readdirSync(root).some(name=>name.startsWith('mcp-context-')),10000,'MCP startup context');
        receipt.mcpStartupContext=fs.readdirSync(root).filter(name=>name.startsWith('mcp-context-'))
          .map(name=>JSON.parse(fs.readFileSync(path.join(root,name),'utf8')));
      }
      if(options.autojoin){
        const claims=path.join(root,'claims.jsonl');
        await waitUntil(()=>fs.existsSync(claims)&&fs.readFileSync(claims,'utf8').trim().split('\n').length===2,15000,'two independent autojoin claims');
        receipt.autojoinClaims=fs.readFileSync(claims,'utf8').trim().split('\n').map(line=>JSON.parse(line));
        assert.equal(new Set(receipt.autojoinClaims.map(claim=>claim.actor)).size,2,'same-cwd roots need distinct Agents');
        assert.deepEqual(receipt.autojoinClaims.map(claim=>claim.threadId).sort(),receipt.inventory.slice().sort());
        assert(receipt.autojoinClaims.every(claim=>!claim.actor.includes(claim.threadId)),'Agent name is not a transcript id');
        const log=path.join(root,'acp.jsonl');
        await waitUntil(()=>fs.existsSync(log)&&fs.readFileSync(log,'utf8').trim().split('\n').filter(line=>JSON.parse(line).method==='_sudocode/a2a/agent_list').length===2,10000,'authenticated ACP identity checks');
        receipt.autojoinAcp=fs.readFileSync(log,'utf8').trim().split('\n').map(line=>JSON.parse(line));
        assert.equal(new Set(receipt.autojoinAcp.map(call=>call.pid)).size,2,'one receiver owner per distinct Agent');
        assert(!receipt.autojoinAcp.some(call=>call.method==='session/prompt'),'autojoin must not start the inner model');
        receipt.autojoinTools=[];
        for(const claim of receipt.autojoinClaims){
          // Actual native MCP routing overwrites a conflicting caller _meta.
          const result=await request(observer,'native MCP routing','mcpServer/tool/call',{
            threadId:claim.threadId,server:'nexus_auto',tool:'scode_send',
            arguments:{to:'fixture-peer',message:'Exact automatic root 🧪\n\u0000',summary:'root binding'},
            _meta:{threadId:receipt.autojoinClaims.find(other=>other!==claim).threadId},
          });
          assert.equal(result.isError,false,JSON.stringify(result));
          assert.equal(JSON.parse(result.content[0].text).routing.sender,claim.actor);
          receipt.autojoinTools.push({threadId:claim.threadId,result});
        }
        const audit=fs.readFileSync(path.join(root,'autojoin-audit.jsonl'),'utf8').trim().split('\n').map(line=>JSON.parse(line));
        receipt.autojoinOwner=audit.filter(row=>row.phase==='listening');
        assert.equal(receipt.autojoinOwner.length,1,'racing MCP entries elect one observer/receiver owner');
        receipt.autojoinCommandAudit=audit.filter(row=>row.phase==='returned');
        assert.equal(receipt.autojoinCommandAudit.length,2);
        assert(receipt.autojoinCommandAudit.every(row=>receipt.autojoinClaims.some(claim=>
          row.nativeThreadId===claim.threadId&&row.nativeAudit.actor===claim.actor)));
        assert.equal(receipt.modelRequests.length,0,'autojoin must not start the native model');
      }
      if(seededResume){
        const original=receipt.created[0];
        // Control the fixture name before seeding so the TUI does not create an
        // unrelated title-generation worker. Every HTTP request still counts.
        receipt.seedThreadName='Native lifecycle fixture';
        await request(observer,'fixture seed','thread/name/set',{threadId:original.id,name:receipt.seedThreadName});
        phase=cancelledResume?'cancelled-seed':'completed-seed';
        const turn=await request(observer,'fixture seed','turn/start',{threadId:original.id,
          input:[{type:'text',text:'Seed isolated native lifecycle history.'}]});
        receipt.seedTurnId=turn.turn.id;
        const end=Date.now()+15000;
        let interrupted=false;
        while(true){
          const {thread}=await request(observer,'fixture seed','thread/read',{threadId:original.id,includeTurns:false});
          const displayed=tuis.some(own=>{
            const p=path.join(root,'tui-'+own.label+'.trace.json');
            return fs.existsSync(p) && JSON.parse(fs.readFileSync(p,'utf8')).outputTail.includes(
              cancelledResume?'Seed isolated native lifecycle history.':'Isolated lifecycle seed response.');
          });
          if(cancelledResume && displayed && thread.status.type==='active' && !interrupted){
            await request(observer,'fixture seed','turn/interrupt',{threadId:original.id,turnId:turn.turn.id});
            interrupted=true;receipt.seedKind='explicit native user turn cancelled after its actual UI history appeared';
          }
          if(thread.status.type==='idle' && displayed && (cancelledResume?interrupted:receipt.modelRequests.length===1))break;
          if(Date.now()>end)throw new Error('Mock seed turn did not complete');
          await new Promise(resolve=>setTimeout(resolve,50));
        }
        phase='post-seed';
        if(!cancelledResume)assert.equal(receipt.modelRequests.length,1,'Only one explicit mock seed inference is allowed');
        receipt.seedModelRequests=receipt.modelRequests.length;
        receipt.cleanup=[];
        for(const own of tuis)await closeTui(own);
        receipt.loadedAfterFrontendClose=(await request(observer,'observer','thread/loaded/list',{})).data;
        phase='resume';
        const resumer=await spawnTui('resume',['resume',original.id]);
        const readyEnd=Date.now()+15000;
        let restored;
        while(true){
          const {thread}=await request(observer,'observer','thread/read',{threadId:original.id,includeTurns:false});
          if(thread.id===original.id && thread.status.type==='idle'){
            const tracePath=path.join(root,'tui-resume.trace.json');
            const screen=fs.existsSync(tracePath)?JSON.parse(fs.readFileSync(tracePath,'utf8')).outputTail:'';
            if(screen.includes(cancelledResume?'Seed isolated native lifecycle history.':'Isolated lifecycle seed response.')){restored=thread;break;}
          }
          if(Date.now()>readyEnd)throw new Error('Actual resume did not restore original ID/history while idle');
          await new Promise(resolve=>setTimeout(resolve,50));
        }
        assert.equal(fs.realpathSync(restored.cwd),fs.realpathSync(workspace),'Resume must retain the canonical workspace');
        receipt.resumed={id:restored.id,cwd:restored.cwd,status:restored.status,actualTuiHistoryRestored:true};
        assert.equal(receipt.modelRequests.length,receipt.seedModelRequests,'Resume must add zero model requests');
        receipt.resumeAdditionalModelRequests=0;
        await closeTui(resumer);
      }
      receipt.result='PASS';
      receipt.notProven=[...(seededResume?[]:['existing-history resume']),...(cancelledResume?['completed assistant-history resume']:[]),
        'automatic authenticated Nexus enrollment',...(process.platform==='win32'?[]:['Windows actual TUI/default endpoint']),
        'current production private frontend migration','all Codex application launch modes'];
      return;
    }
  } catch(error){receipt.result='FAIL';receipt.error={name:error.name,message:error.message};process.exitCode=1;}
  finally {
    phase='cleanup';
    for(const response of heldResponses)response.destroy();
    heldResponses.clear();
    receipt.cleanup??=[];
    for(const own of tuis){
      try{await closeTui(own);}catch(error){receipt.cleanup.push(String(error));receipt.result='FAIL';process.exitCode=1;}
    }
    for(const client of connections.reverse()){
      try{await client.close();receipt.cleanup.push('own proxy closed');}
      catch(error){receipt.cleanup.push(String(error));receipt.result='FAIL';process.exitCode=1;}
    }
    if(native){
      native.kill('SIGTERM');
      try{await bounded(nativeExited,5000,'isolated native exit');}
      catch(error){native.kill('SIGKILL');try{await bounded(nativeExited,3000,'isolated native forced exit');}catch(e){receipt.cleanup.push(String(e));receipt.result='FAIL';process.exitCode=1;}}
      receipt.cleanup.push({nativeExitCode:native.exitCode,nativeSignalCode:native.signalCode});
      // Windows maps SIGTERM to TerminateProcess too; its signal name cannot
      // establish graceful shutdown, even without reaching the SIGKILL phase.
      if(process.platform==='win32' || native.signalCode==='SIGKILL')receipt.notProven=[...new Set([...(receipt.notProven??[]),'graceful native server shutdown'])];
    }
    if(receipt.autojoinOwner){
      try{
        await waitUntil(()=>receipt.autojoinOwner.every(owner=>{
          try{process.kill(owner.pid,0);return false;}catch(error){if(error.code==='ESRCH')return true;throw error;}
        }),10000,'last MCP entry closes its shared owner');
        receipt.cleanup.push('shared autojoin owner exited');
      }catch(error){receipt.cleanup.push(String(error));receipt.result='FAIL';process.exitCode=1;}
    }
    await new Promise(resolve=>endpoint.close(resolve));
    receipt.nativeStderr=nativeStderr;
    const name='native-codex-lifecycle-'+receipt.at.replace(/[:.]/g,'')+'.receipt.json';
    fs.writeFileSync(path.join(evidence,name),JSON.stringify(receipt,null,2)+'\n',{mode:0o600,flag:'wx'});
    console.log(JSON.stringify({result:receipt.result,error:receipt.error,receipt:path.join(evidence,name),created:receipt.created,startedSeenByObserver:receipt.startedSeenByObserver,modelRequests:receipt.modelRequests.length,cleanup:receipt.cleanup}));
  }
})();
