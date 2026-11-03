import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn, execFileSync } from 'node:child_process';
import { once } from 'node:events';

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
  const tasks:Array<{taskName:string;config:string}>=[];
  const fixture=(name:string,extra:Record<string,string>={})=>{
    const work=path.join(dir,name);fs.mkdirSync(work,{mode:0o700});
    const home=path.join(work,'home');fs.mkdirSync(home,{mode:0o700});
    const target=path.join(work,windows?'codex.ps1':'codex');
    fs.writeFileSync(target,windows?'Write-Output original\r\n':'#!/bin/sh\nexit 19\n',{mode:0o755});
    const env:Record<string,string>={CODEX_HOME:home,HOME:home,USERPROFILE:home,SCODE_SETUP_FAKE_ROOT:work,...extra};
    for(const name of ['PATH','SystemRoot','TEMP','TMP','TMPDIR'])if(process.env[name])env[name]=process.env[name]!;
    const config=path.join(work,'runtime.json');
    const envFile=path.join(work,'native-env.json');fs.writeFileSync(envFile,JSON.stringify(env),{mode:0o600});
    const prepared=JSON.parse(execFileSync(process.execPath,[server,'--init-codex','--config',config,
      '--native-argv',JSON.stringify([fs.realpathSync.native(process.execPath),native]),
      '--native-env-file',envFile,'--enrollment',JSON.stringify([process.execPath,'never-enroll-in-this-install-test']),
      '--peers','[]','--command-entry',target],{encoding:'utf8',windowsHide:true}));
    assert.deepEqual(prepared.installationArgv,[process.execPath,server,'--install-codex','--config',config,'--command-entry',target]);
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
    return {work,home,target,config,calls,install};
  };
  try{
    const good=fixture('happy space');const before=fs.readFileSync(good.target);
    const result=await good.install();assert.equal(result.code,0,result.stderr);
    const report=JSON.parse(result.stdout);assert.equal(report.installed,true);
    assert.equal(report.a2aJourneyVerified,false);assert.equal(report.currentTerminal,'not inferred');
    const journal=JSON.parse(fs.readFileSync(report.journal,'utf8'));
    assert.equal(journal.phase,'installed');assert.equal(journal.inFlight,undefined);
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

    const partial=fixture('lost-result',{SCODE_SETUP_LOST_REGISTRATION:'1'});
    const failed=await partial.install();assert.equal(failed.code,1);assert(fs.existsSync(path.join(partial.work,'registration.json')));
    const attempt=JSON.parse(fs.readFileSync(partial.config+'.installation.json','utf8'));
    assert.equal(attempt.inFlight,'globalMcpRegistration');assert.equal(attempt.phase,'stopped');
    const count=partial.calls().filter(c=>c[1]==='add').length;
    assert.equal((await partial.install()).code,1);assert.equal(partial.calls().filter(c=>c[1]==='add').length,count);
    console.log('Codex setup process integration PASS: one command installs canonical MCP/entry; actual delegate exit/argv and backup; conflict/racing edit preserved; lost result and completed setup not replayed');
  }finally{
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
