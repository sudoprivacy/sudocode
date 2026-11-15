import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn, execFileSync } from 'node:child_process';
import { once } from 'node:events';
import { WebSocketServer } from 'ws';

const root=path.resolve(__dirname,'../..');
const server=path.join(root,'dist/server.cjs');
const native=path.join(root,'test/fixtures/codex-setup-native.mjs');
const windows=process.platform==='win32';
const system=(name:string)=>path.join(process.env.SystemRoot!,'System32',name);

async function run(argv:string[],env=process.env):Promise<{code:number|null;stdout:string;stderr:string}>{
  const child=spawn(argv[0],argv.slice(1),{env,shell:false,windowsHide:true,stdio:['ignore','pipe','pipe']});
  let stdout='',stderr='';child.stdout.on('data',b=>stdout+=String(b));child.stderr.on('data',b=>stderr+=String(b));
  const [code]=await once(child,'close');return {code,stdout,stderr};
}

async function main():Promise<void>{
  const dir=fs.mkdtempSync(path.join(os.tmpdir(),'codex-setup-process-'));
  const nativeServer=new WebSocketServer({host:'127.0.0.1',port:0});
  await once(nativeServer,'listening');
  const address=nativeServer.address();assert(address&&typeof address!=='string');
  let capability:unknown=1;
  const nativeMethods:string[]=[];
  let changeDuringHandshake:string|undefined;
  let handshakeEdits=0;
  nativeServer.on('connection',socket=>socket.on('message',bytes=>{
    const call=JSON.parse(bytes.toString());nativeMethods.push(call.method);
    if(call.method==='initialize'){
      if(changeDuringHandshake){
        const config=JSON.parse(fs.readFileSync(changeDuringHandshake,'utf8'));
        config.peers=[`independent-native-handshake-edit-${++handshakeEdits}`];
        fs.writeFileSync(changeDuringHandshake,JSON.stringify(config));
      }
      socket.send(JSON.stringify({id:call.id,result:{userAgent:'native-setup-process-fixture',
        ...(capability===undefined?{}:{serverCapabilities:{durableQueueAddOnce:capability}})}}));
    }else assert.equal(call.method,'initialized','Preflight must not discover threads or submit input');
  }));
  const tasks:Array<{taskName:string;config:string}>=[];
  const fixture=(name:string,extra:Record<string,string>={})=>{
    const work=path.join(dir,name);fs.mkdirSync(work,{mode:0o700});
    const home=path.join(work,'home');fs.mkdirSync(home,{mode:0o700});
    const target=path.join(work,windows?'codex.ps1':'codex');
    fs.writeFileSync(target,windows?'Write-Output original\r\n':'#!/bin/sh\nexit 19\n',{mode:0o755});
    const env:Record<string,string>={CODEX_HOME:home,HOME:home,USERPROFILE:home,SCODE_SETUP_FAKE_ROOT:work,
      SCODE_SETUP_PROXY_PORT:String(address.port),...extra};
    for(const name of ['PATH','SystemRoot','TEMP','TMP','TMPDIR'])if(process.env[name])env[name]=process.env[name]!;
    const config=path.join(work,'runtime.json');
    const envFile=path.join(work,'native-env.json');fs.writeFileSync(envFile,JSON.stringify(env),{mode:0o600});
    const prepared=JSON.parse(execFileSync(process.execPath,[server,'--init-codex','--config',config,
      '--native-argv',JSON.stringify([fs.realpathSync.native(process.execPath),native]),
      '--native-env-file',envFile,'--enrollment',JSON.stringify([process.execPath,'never-enroll-in-this-install-test']),
      '--peers','[]','--command-entry',target],{encoding:'utf8',windowsHide:true}));
    assert.deepEqual(prepared.installationArgv,[process.execPath,server,'--install-codex','--config',config,'--command-entry',target]);
    const runtime=JSON.parse(fs.readFileSync(config,'utf8'));
    runtime.native.socket='fixture-setup.sock';
    runtime.native.requireDurableQueue=false; // Operator config cannot disable installation admission.
    fs.writeFileSync(config,JSON.stringify(runtime));
    const calls=()=>fs.existsSync(path.join(work,'calls.jsonl'))
      ? fs.readFileSync(path.join(work,'calls.jsonl'),'utf8').trim().split('\n').filter(Boolean).map(x=>JSON.parse(x)) as string[][]:[];
    const install=async()=>{
      const result=await run(prepared.installationArgv);
      if(fs.existsSync(config+'.installation.json')){
        const journal=JSON.parse(fs.readFileSync(config+'.installation.json','utf8'));
        if(journal.ownerTask && !tasks.some(t=>t.taskName===journal.ownerTask.taskName))tasks.push({taskName:journal.ownerTask.taskName,config});
      }
      return result;
    };
    const preflight=()=>run([process.execPath,server,'--check-codex-native','--config',config]);
    return {work,home,target,config,calls,install,preflight};
  };
  try{
    const good=fixture('happy space');const before=fs.readFileSync(good.target);
    const result=await good.install();assert.equal(result.code,0,result.stderr);
    const report=JSON.parse(result.stdout);assert.equal(report.installed,true);
    assert.equal(report.a2aJourneyVerified,false);assert.equal(report.currentTerminal,'not inferred');
    const journal=JSON.parse(fs.readFileSync(report.journal,'utf8'));
    assert.equal(journal.phase,'installed');assert.equal(journal.inFlight,undefined);
    assert(journal.completed.includes('officialNativeReadback'));
    assert(journal.completed.includes('nativeReceiverPreflight'));
    const commands=good.calls();
    const proxy=commands.findIndex(c=>c[0]==='app-server'&&c[1]==='proxy');
    assert(proxy>=0&&proxy<commands.findIndex(c=>c[0]==='mcp'&&c[1]==='add'));
    assert.deepEqual(commands[proxy],['app-server','proxy','--sock','fixture-setup.sock']);
    assert(Buffer.from(journal.backup.bytes,'base64').equals(before));
    const args=['resume','original-id','a prompt with spaces','semi;colon'];
    const launch=windows
      ? [system('WindowsPowerShell/v1.0/powershell.exe'),'-NoProfile','-NonInteractive','-ExecutionPolicy','Bypass','-File',good.target,...args]
      : [good.target,...args];
    const entered=await run(launch);assert.equal(entered.code,17,entered.stderr);
    assert.deepEqual(JSON.parse(entered.stdout).foregroundArgs,args);
    const writes=good.calls().filter(c=>c[0]==='mcp'&&c[1]==='add').length;assert.equal(writes,1);
    const again=await good.install();assert.equal(again.code,1);assert.match(again.stderr,/installation record already exists/);
    assert.equal(good.calls().filter(c=>c[0]==='mcp'&&c[1]==='add').length,writes,'No replay of a completed setup');

    const check=fixture('check-only');
    const checkEntry=fs.readFileSync(check.target),checkConfig=fs.readFileSync(check.config);
    const checked=await check.preflight();assert.equal(checked.code,0,checked.stderr);
    assert.deepEqual(JSON.parse(checked.stdout),{checked:true,requiredCapability:'durableQueueAddOnce',version:1,a2aJourneyVerified:false});
    assert.deepEqual(check.calls(),[['app-server','proxy','--sock','fixture-setup.sock']]);
    assert(fs.readFileSync(check.target).equals(checkEntry));assert(fs.readFileSync(check.config).equals(checkConfig));
    assert(!fs.existsSync(check.config+'.installation.json'));
    assert(!fs.existsSync(path.join(check.home,'.sudocode-a2a-installation.json')));
    assert(!fs.existsSync(path.join(check.work,'registration.json')));

    for(const [label,value] of [['missing',undefined],['wrong-version','1']] as const){
      capability=value;
      const unsupported=fixture('capability-'+label);
      const entry=fs.readFileSync(unsupported.target),runtime=fs.readFileSync(unsupported.config);
      const checked=await unsupported.preflight();assert.equal(checked.code,1);
      assert.match(checked.stderr,/did not negotiate durable/);
      assert(!fs.existsSync(unsupported.config+'.installation.json'));
      const stopped=await unsupported.install();assert.equal(stopped.code,1);
      const attempt=JSON.parse(fs.readFileSync(unsupported.config+'.installation.json','utf8'));
      assert.equal(attempt.inFlight,'nativeReceiverPreflight');assert.equal(attempt.phase,'stopped');
      assert.match(attempt.error,/did not negotiate durable/);assert.equal(attempt.ownerTask,undefined);
      assert(fs.readFileSync(unsupported.target).equals(entry));assert(fs.readFileSync(unsupported.config).equals(runtime));
      assert(!fs.existsSync(path.join(unsupported.work,'registration.json')));
      assert.equal(unsupported.calls().filter(c=>c[0]==='mcp'&&c[1]==='add').length,0);
    }
    capability=1;
    const unreachable=fixture('native-connection-refused',{SCODE_SETUP_PROXY_REFUSAL:'1'});
    const unreachableEntry=fs.readFileSync(unreachable.target);
    assert.equal((await unreachable.install()).code,1);
    const unreachableAttempt=JSON.parse(fs.readFileSync(unreachable.config+'.installation.json','utf8'));
    assert.equal(unreachableAttempt.inFlight,'nativeReceiverPreflight');assert.equal(unreachableAttempt.ownerTask,undefined);
    assert(fs.readFileSync(unreachable.target).equals(unreachableEntry));
    assert(!fs.existsSync(path.join(unreachable.work,'registration.json')));

    const handshakeEdit=fixture('native-handshake-runtime-edit');
    changeDuringHandshake=handshakeEdit.config;
    const handshakeEntry=fs.readFileSync(handshakeEdit.target);
    assert.equal((await handshakeEdit.preflight()).code,1);
    assert(!fs.existsSync(handshakeEdit.config+'.installation.json'));
    assert.equal((await handshakeEdit.install()).code,1);
    const editedAttempt=JSON.parse(fs.readFileSync(handshakeEdit.config+'.installation.json','utf8'));
    assert.match(editedAttempt.error,/runtime changed during installation/);
    assert.equal(editedAttempt.ownerTask,undefined);
    assert(fs.readFileSync(handshakeEdit.target).equals(handshakeEntry));
    assert(!fs.existsSync(path.join(handshakeEdit.work,'registration.json')));
    changeDuringHandshake=undefined;

    const conflict=fixture('conflict');const original=fs.readFileSync(conflict.target);
    fs.writeFileSync(path.join(conflict.work,'registration.json'),JSON.stringify({name:'nexus_scode',transport:{type:'stdio',command:'another-owner'}}));
    const refused=await conflict.install();assert.equal(refused.code,1);assert.match(refused.stderr,/explicit migration is required/);
    assert(fs.readFileSync(conflict.target).equals(original));assert(!fs.existsSync(conflict.config+'.installation.json'));
    assert.equal(conflict.calls().filter(c=>c[0]==='app-server'||c[1]==='add').length,0);

    const raced=fixture('entry-change');
    const raceConfig=JSON.parse(fs.readFileSync(raced.config,'utf8'));
    raceConfig.native.env.SCODE_SETUP_REPLACE_ENTRY=raced.target;fs.writeFileSync(raced.config,JSON.stringify(raceConfig));
    const lost=await raced.install();assert.equal(lost.code,1);assert.match(lost.stderr,/installation stopped/);
    assert.equal(fs.readFileSync(raced.target,'utf8'),'independent edit\n');
    assert.match(JSON.parse(fs.readFileSync(raced.config+'.installation.json','utf8')).error,/independent edit is preserved/);

    const runtimeRace=fixture('runtime-change-during-preflight');
    const runtimeOriginalEntry=fs.readFileSync(runtimeRace.target);
    const runtimeConfig=JSON.parse(fs.readFileSync(runtimeRace.config,'utf8'));
    runtimeConfig.native.env.SCODE_SETUP_REPLACE_RUNTIME=runtimeRace.config;
    fs.writeFileSync(runtimeRace.config,JSON.stringify(runtimeConfig));
    const changedRuntime=await runtimeRace.install();assert.equal(changedRuntime.code,1);
    assert.match(changedRuntime.stderr,/runtime changed during preparation/);
    assert.deepEqual(JSON.parse(fs.readFileSync(runtimeRace.config,'utf8')).peers,['independent-user-setting']);
    assert(fs.readFileSync(runtimeRace.target).equals(runtimeOriginalEntry));
    assert(!fs.existsSync(runtimeRace.config+'.installation.json'));
    assert(!fs.existsSync(path.join(runtimeRace.home,'.sudocode-a2a-installation.json')));
    assert.equal(runtimeRace.calls().filter(c=>c[0]==='app-server'||c[1]==='add').length,0);

    const unhealthy=fixture('start-succeeded-but-daemon-stopped',{SCODE_SETUP_UNHEALTHY_DAEMON:'1'});
    const unhealthyEntry=fs.readFileSync(unhealthy.target);
    const noDaemon=await unhealthy.install();assert.equal(noDaemon.code,1);
    const unhealthyJournal=JSON.parse(fs.readFileSync(unhealthy.config+'.installation.json','utf8'));
    assert.equal(unhealthyJournal.phase,'stopped');
    assert.match(unhealthyJournal.error,/daemon readback did not report running/);
    assert(fs.readFileSync(unhealthy.target).equals(unhealthyEntry));
    assert.equal(unhealthy.calls().filter(c=>c[1]==='add').length,0);

    const partial=fixture('lost-result',{SCODE_SETUP_LOST_REGISTRATION:'1'});
    const failed=await partial.install();assert.equal(failed.code,1);assert(fs.existsSync(path.join(partial.work,'registration.json')));
    const attempt=JSON.parse(fs.readFileSync(partial.config+'.installation.json','utf8'));
    assert.equal(attempt.inFlight,'globalMcpRegistration');assert.equal(attempt.phase,'stopped');
    const count=partial.calls().filter(c=>c[1]==='add').length;
    assert.equal((await partial.install()).code,1);assert.equal(partial.calls().filter(c=>c[1]==='add').length,count);
    assert(nativeMethods.includes('initialize'));
    assert(nativeMethods.every(method=>method==='initialize'||method==='initialized'));
    console.log('Codex setup process integration PASS: actual proxy negotiation precedes global writes; missing/wrong capability and transport refusal preserve config/entry with no owner Task; read-only preflight and handshake edit refusal; canonical MCP/delegate, running readback, conflicts and uncertain setup retained');
  }finally{
    for(const socket of nativeServer.clients)socket.terminate();
    await new Promise<void>((resolve,reject)=>nativeServer.close(error=>error?reject(error):resolve()));
    if(windows)for(const task of tasks){
      const xml=execFileSync(system('schtasks.exe'),['/Query','/TN',task.taskName,'/XML'],{windowsHide:true});
      const text=xml.toString(xml[0]===0xff?'utf16le':'utf8');
      assert(text.includes(task.config),'Only an exact private test action may be removed');
      execFileSync(system('schtasks.exe'),['/Delete','/TN',task.taskName,'/F'],{windowsHide:true});
    }
    fs.rmSync(dir,{recursive:true,force:true});
  }
}
void main().catch(error=>{console.error(error);process.exitCode=1;});
