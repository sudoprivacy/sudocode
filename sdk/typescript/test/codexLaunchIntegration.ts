import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { WebSocketServer } from 'ws';

async function main():Promise<void>{
  const root=path.resolve(__dirname,'../..');
  const dir=fs.mkdtempSync(path.join(os.tmpdir(),'codex-user-entry-'));
  const calls=path.join(dir,'calls.jsonl');
  const config=path.join(dir,'runtime.json');
  const server=new WebSocketServer({host:'127.0.0.1',port:0});
  await once(server,'listening');const address=server.address();assert(address&&typeof address!=='string');
  let reviewer:unknown='user';const reads:unknown[]=[];
  server.on('connection',socket=>socket.on('message',bytes=>{
    const call=JSON.parse(String(bytes));
    if(call.method==='initialized')return;
    assert(['initialize','config/read'].includes(call.method),'The launcher must never submit a task, tool, or enrollment');
    if(call.method==='config/read')reads.push(call.params);
    socket.send(JSON.stringify({jsonrpc:'2.0',id:call.id,result:call.method==='initialize'
      ?{userAgent:'launch-process-fixture'}:{config:{approvals_reviewer:reviewer}}}));
  }));
  const native=[process.execPath,path.join(root,'test/fixtures/codex-launch-native.mjs')];
  fs.writeFileSync(config,JSON.stringify({version:1,mode:'codex-autojoin',native:{codexArgv:native},
    stateDirectory:path.join(dir,'bindings'),auditLog:path.join(dir,'audit.jsonl'),peers:[],
    enrollmentArgv:[process.execPath,'never-run'],endpoint:{port:23262,token:'a'.repeat(64)}}));
  const run=async(args:string[])=>{
    fs.writeFileSync(calls,'');const before=reads.length;
    const child=spawn(process.execPath,[path.join(root,'dist/server.cjs'),'--launch-codex','--config',config,'--',...args],
      {env:{...process.env,SCODE_LAUNCH_CALLS:calls,SCODE_LAUNCH_PORT:String(address.port)},stdio:['ignore','pipe','pipe']});
    let stdout='',stderr='';child.stdout.on('data',b=>stdout+=String(b));child.stderr.on('data',b=>stderr+=String(b));
    const [code]=await once(child,'close');
    return {code,stdout,stderr,reads:reads.slice(before),calls:fs.readFileSync(calls,'utf8').trim().split('\n').filter(Boolean).map(x=>JSON.parse(x)) as string[][]};
  };
  try{
    for(const configArgs of [['-c','approvals_reviewer=user'],['--config','approvals_reviewer="user"'],
      ['--config=approvals_reviewer=user'],["-capprovals_reviewer='user'"]]){
      const result=await run(['--sandbox','danger-full-access',...configArgs,'resume','original-id']);
      assert.equal(result.code,17,result.stderr);
      assert.deepEqual(result.calls.at(-1),['--sandbox','danger-full-access','resume','original-id']);
      assert.equal(result.calls.filter(x=>x.slice(-3).join(' ')==='app-server daemon start').length,1);
      assert.equal(result.reads.length,1);
      assert.match(result.stderr,/user review is already configured/);
    }
    for(const args of [['exec','-c','approvals_reviewer=user','a task'],['--help'],
      ['--no-daemon','-c','approvals_reviewer=user'],['-c','approvals_reviewer=auto_review'],
      ['-c','model="other"','-c','approvals_reviewer=user'],['--','-c','approvals_reviewer=user']]){
      const result=await run(args);assert.equal(result.code,17);
      assert.deepEqual(result.calls,[args]);assert.deepEqual(result.reads,[]);
    }
    const cd=await run(['--cd',dir,'-c','approvals_reviewer=user','resume','original-id']);
    assert.deepEqual(cd.reads,[{cwd:dir,includeLayers:false}]);
    reviewer='auto_review';const refused=await run(['-c','approvals_reviewer=user','resume','original-id']);
    assert.equal(refused.code,1);assert.match(refused.stderr,/refusing to remove/);
    assert.equal(refused.calls.length,2,'Only official preparation and read-only proxy may run before refusal');
    reviewer=undefined;const missing=await run(['-c','approvals_reviewer=user']);assert.equal(missing.code,1);
    assert.match(missing.stderr,/refusing to remove/);
    assert.equal(fs.existsSync(path.join(dir,'bindings')),false);
    assert.equal(fs.existsSync(path.join(dir,'audit.jsonl')),false);
    console.log('Codex user entry process integration PASS: redundant review preserves real config, argv, exit status; conflicting/missing policy refuses; other commands remain native');
  }finally{
    for(const socket of server.clients)socket.terminate();
    await new Promise<void>(resolve=>server.close(()=>resolve()));
    fs.rmSync(dir,{recursive:true,force:true});
  }
}
void main().catch(error=>{console.error(error);process.exitCode=1;});
