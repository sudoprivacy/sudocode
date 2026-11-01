// Explicit opt-in real native PTY journey; no user home, credentials or Nexus.
const assert=require('node:assert/strict'),fs=require('node:fs'),os=require('node:os'),path=require('node:path');
const http=require('node:http'),crypto=require('node:crypto');
const {spawn,spawnSync}=require('node:child_process');
const {parseArgs}=require('node:util');
const {values}=parseArgs({options:{'native-argv':{type:'string'},output:{type:'string'}}});
const argv=JSON.parse(values['native-argv']||'null');
assert(Array.isArray(argv)&&argv.length&&argv.every(x=>typeof x==='string'),'Select the actual official native argv explicitly');
assert(values.output&&path.isAbsolute(values.output),'Keep an absolute receipt outside the private home');
assert(!fs.existsSync(values.output),'Preserve the original attempt, not an automatic rerun');
const root=fs.mkdtempSync(path.join(os.tmpdir(),'codex-human-review-entry-'));
const home=path.join(root,'home'),work=path.join(root,'work'),user=path.join(root,'user');
for(const p of [home,work,user])fs.mkdirSync(p);
for(let p=work;;p=path.dirname(p)){
  assert(!fs.existsSync(path.join(p,'AGENTS.md'))&&!fs.existsSync(path.join(p,'.nexus/sudocode/AGENTS.md')),'Private fixture must not inherit instructions');
  if(p===path.dirname(p))break;
}
const sdk=path.resolve(__dirname,'..'),entry=path.join(sdk,'dist/server.cjs');
const library=require(path.join(sdk,'dist/index.cjs'));
const driver=path.join(__dirname,'fixtures',process.platform==='win32'?'nativeCodexTui.windows.py':'nativeCodexTui.py');
const python=process.env.SCODE_TEST_PYTHON||(process.platform==='win32'?'python':'python3');
const state={at:new Date().toISOString(),state:'STARTING_PRIVATE_NATIVE_USER_ENTRY',
  nativeArgv:argv,source:JSON.parse(fs.readFileSync(path.join(sdk,'manifest.json'),'utf8')),
  realUserHomeUsed:false,nexusUsed:false,promptsSubmitted:false,modelRequests:[],frontends:[],forcedCleanup:[]};
const save=()=>fs.writeFileSync(values.output,JSON.stringify(state,null,2)+'\n',{mode:0o600});save();
const env={};for(const key of ['PATH','SystemRoot','WINDIR','COMSPEC','PATHEXT','TMPDIR','TEMP','TMP','LANG','LC_ALL'])if(process.env[key]!==undefined)env[key]=process.env[key];
Object.assign(env,{HOME:user,USERPROFILE:user,CODEX_HOME:home,TERM:'xterm-256color',
  APPDATA:path.join(root,'roaming'),LOCALAPPDATA:path.join(root,'local'),XDG_CONFIG_HOME:path.join(root,'xdg'),
  NPM_CONFIG_PREFIX:path.join(root,'npm'),NO_PROXY:'127.0.0.1,localhost',no_proxy:'127.0.0.1,localhost'});
const mock=http.createServer((req,res)=>{state.modelRequests.push({method:req.method,url:req.url});save();res.writeHead(503);res.end('No inference expected');});
const bounded=(promise,ms,label)=>new Promise((resolve,reject)=>{const timer=setTimeout(()=>reject(Error(label+' timeout')),ms);promise.then(x=>{clearTimeout(timer);resolve(x);},e=>{clearTimeout(timer);reject(e);});});
const sleep=ms=>new Promise(resolve=>setTimeout(resolve,ms));
const native=(args)=>{const result=spawnSync(argv[0],[...argv.slice(1),...args],{env,cwd:work,encoding:'utf8',timeout:35000});return {args,status:result.status,stdout:result.stdout,stderr:result.stderr,error:result.error?.message};};
let observer,active;
async function closeTui(){
  active.child.stdin.end(JSON.stringify({action:'close'})+'\n');
  assert.equal(await bounded(active.exited,15000,'own normal frontend exit'),0,JSON.stringify(active.row));
  assert(active.row.events.some(x=>x.event==='quit_sent'));
  assert(!active.row.events.some(x=>['terminate','kill','unsafe_update_prompt','normal_exit_timeout'].includes(x.event)));
  active=undefined;
}
async function startTui(label,args){
  const row={label,events:[]};state.frontends.push(row);
  const trace=path.join(root,label+'.trace.json');
  const launch=[driver,process.execPath,entry,'--launch-codex','--config',path.join(root,'entry.json'),'--',
    '--sandbox','danger-full-access','--ask-for-approval','on-request','-c','approvals_reviewer=user',...args];
  row.argv=[python,...launch];save();
  const child=spawn(python,launch,{cwd:work,env:{...env,NATIVE_LIFECYCLE_PTY_TRACE:trace},stdio:['pipe','pipe','pipe']});
  row.pid=child.pid;let buffer='';
  child.stdout.on('data',bytes=>{buffer+=String(bytes);let i;while((i=buffer.indexOf('\n'))>=0){const line=buffer.slice(0,i);buffer=buffer.slice(i+1);if(line)row.events.push(JSON.parse(line));save();}});
  child.stderr.on('data',bytes=>{row.stderr=(row.stderr||'')+String(bytes);save();});
  const exited=new Promise(resolve=>child.once('close',(code,signal)=>{row.exitCode=code;row.signal=signal;save();resolve(code);}));
  active={child,row,exited};const deadline=Date.now()+30000;
  for(;;){
    assert.equal(row.exitCode,undefined,'Private frontend exited before native inventory');
    if(!observer){try{observer=await library.connectCodex({codexArgv:argv,env});}catch(error){row.observerFirstError=String(error);}}
    if(observer){
      const loaded=await observer.request('thread/loaded/list',{});assert.equal(loaded.nextCursor,null);
      if(loaded.data.length===1){
        const {thread}=await observer.request('thread/read',{threadId:loaded.data[0],includeTurns:false});
        let output;try{output=JSON.parse(fs.readFileSync(trace,'utf8')).outputTail;}catch{}
        if(thread.threadSource==='user'&&thread.parentThreadId===null&&thread.canAcceptDirectInput===true&&output?.includes('›')){
          assert(output.includes('user review is already configured'));
          row.threadId=thread.id;row.sharedRootMetadata={cwd:thread.cwd,status:thread.status,threadSource:thread.threadSource,parentThreadId:thread.parentThreadId,canAcceptDirectInput:thread.canAcceptDirectInput};
          row.outputTail=output;save();return thread.id;
        }
      }
    }
    assert(Date.now()<deadline,'Private shared native root not observed');await sleep(100);
  }
}
(async()=>{try{
  await new Promise(resolve=>mock.listen(0,'127.0.0.1',resolve));
  const port=mock.address().port;
  fs.writeFileSync(path.join(home,'config.toml'),`approvals_reviewer = "user"\nmodel = "entry-fixture"\nmodel_provider = "entry-fixture"\n[model_providers.entry-fixture]\nname = "Owned idle fixture"\nbase_url = "http://127.0.0.1:${port}/v1"\nwire_api = "responses"\nexperimental_bearer_token = "fixture-only"\n[projects.${JSON.stringify(fs.realpathSync(work))}]\ntrust_level = "trusted"\n`);
  state.privateStatePreparation=native(['app-server','daemon','stop']);assert.equal(state.privateStatePreparation.status,0);
  assert.equal(JSON.parse(state.privateStatePreparation.stdout).status,'notRunning');
  fs.writeFileSync(path.join(home,'app-server-daemon/settings.json'),JSON.stringify({remoteControlEnabled:false,updater:{autoUpdateEnabled:false},shutdownGraceSeconds:5}),{mode:0o600,flag:'wx'});
  fs.writeFileSync(path.join(root,'entry.json'),JSON.stringify({version:1,mode:'codex-autojoin',native:{codexArgv:argv,env},
    stateDirectory:path.join(root,'never-enrolled'),auditLog:path.join(root,'never-audit.jsonl'),peers:[],enrollmentArgv:[process.execPath,'never-run'],endpoint:{port:23262,token:'a'.repeat(64)}}),{mode:0o600});
  state.nativeVersion=native(['--version']);assert.equal(state.nativeVersion.status,0);
  const first=await startTui('new',[]);await closeTui();
  const resumed=await startTui('resume',['resume',first]);assert.equal(resumed,first);await closeTui();
  state.managedVersion=native(['app-server','daemon','version']);assert.equal(state.managedVersion.status,0);
  assert.equal(state.modelRequests.length,0);assert(!fs.existsSync(path.join(root,'never-enrolled')));
  state.state='PASS_REAL_NATIVE_OLD_USER_COMMAND_SHARED_NEW_AND_RESUME';
}catch(error){state.state='FAILED_RETAINED';state.error=String(error);process.exitCode=1;}
finally{
  if(active){try{await closeTui();}catch(error){state.cleanupError=String(error);}}
  await observer?.close().catch(error=>{state.observerCloseError=String(error);});
  state.privateDaemonStop=native(['app-server','daemon','stop']);
  await new Promise(resolve=>mock.close(resolve));
  // Preserve receipt/trace tails; no private authentication or daemon package.
  if(state.privateDaemonStop.status===0&&!active)fs.rmSync(root,{recursive:true,force:true});
  else state.retainedPrivateRoot=root;
  state.finishedAt=new Date().toISOString();save();console.log(JSON.stringify({state:state.state,error:state.error,frontends:state.frontends.map(({label,threadId,exitCode})=>({label,threadId,exitCode})),modelRequests:state.modelRequests,retainedPrivateRoot:state.retainedPrivateRoot},null,2));
}})();
