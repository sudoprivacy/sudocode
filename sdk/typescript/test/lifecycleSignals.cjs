// Public packaged lifecycle, real subprocesses and real endpoint exclusion.
const assert=require('node:assert/strict'),path=require('node:path');
const {fork}=require('node:child_process'),{createServer}=require('node:net');
const {once}=require('node:events'),{setTimeout:delay}=require('node:timers/promises');
async function deadline(value){
  let timer;
  try{return await Promise.race([value,new Promise((_,reject)=>{timer=setTimeout(()=>reject(Error('Signal cleanup acceptance deadline')),5000);})]);}
  finally{clearTimeout(timer);}
}
async function run(mode){
  const child=fork(path.join(__dirname,'fixtures/lifecycle-owner.cjs'),[mode],{silent:true,windowsHide:true});
  let stderr='';child.stderr.on('data',bytes=>{stderr+=String(bytes);});
  const exit=once(child,'close');
  try{
    const [ready]=await deadline(once(child,'message'));assert.equal(ready.phase,'ready');
    const observed=once(child,'message');child.send('SIGTERM');
    assert.equal((await deadline(observed))[0].phase,'child_closed');
    assert.throws(()=>process.kill(ready.childPid,0),{code:'ESRCH'});
    if(mode==='clean'){
      const [code,signal]=await deadline(exit);assert.equal(code,143,stderr);assert.equal(signal,null);
      const available=createServer();available.listen(ready.port,'127.0.0.1');await deadline(once(available,'listening'));
      await new Promise(resolve=>available.close(resolve));
    }else{
      await delay(100);assert.equal(child.exitCode,null,stderr);
      assert.match(stderr,/Shutdown refused; cleanup UNKNOWN/);
      const duplicate=createServer();const refused=once(duplicate,'error');duplicate.listen(ready.port,'127.0.0.1');
      assert.equal((await deadline(refused))[0].code,'EADDRINUSE');duplicate.close();
      child.send('SIGINT');await delay(100);
      assert.equal(child.exitCode,null,'Second graceful signal cannot convert UNKNOWN to success');
      assert.equal(stderr.match(/Shutdown refused/g)?.length,2);
    }
  }finally{
    if(child.exitCode===null&&child.signalCode===null)child.kill('SIGKILL');
    await deadline(exit);
  }
}
void (async()=>{
  await run('clean');await run('unknown');
  console.log('PASS packaged signal adapter: observed child exit, clean release, UNKNOWN endpoint retained, repeated signal refusal');
  console.log('Typed error injected after real child exit; Node signal event via IPC, not forced Windows OS termination proof');
})().catch(error=>{console.error(error);process.exitCode=1;});
