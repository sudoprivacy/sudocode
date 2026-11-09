// Real child + exclusive endpoint, using the packaged CLI lifecycle adapter.
// IPC enters Node's signal event boundary: Windows OS SIGTERM is forced kill
// and cannot exercise an asynchronous graceful handler.
const {spawn}=require('node:child_process');
const {once}=require('node:events');
const {createServer}=require('node:net');
const {registerCommandShutdown,installCommandShutdownSignals,ScodeCleanupUnknownError}=require('../../dist/index.cjs');
async function main(){
  const child=spawn(process.execPath,['-e','process.stdin.resume();process.stdin.on("end",()=>process.exit(0));'],
    {windowsHide:true,stdio:['pipe','ignore','ignore']});
  const closed=once(child,'close');
  const lock=createServer(socket=>socket.destroy());
  lock.listen(0,'127.0.0.1');await once(lock,'listening');
  const failure=new ScodeCleanupUnknownError('acceptance typed cleanup failure after real child exit');
  registerCommandShutdown(async()=>{
    child.stdin.end();await closed;
    process.send({phase:'child_closed',pid:child.pid});
    if(process.argv[2]==='unknown')throw failure;
    await new Promise((resolve,reject)=>lock.close(error=>error?reject(error):resolve()));
  });
  installCommandShutdownSignals();
  process.on('message',value=>{if(value==='SIGTERM'||value==='SIGINT')process.emit(value);});
  process.send({phase:'ready',port:lock.address().port,pid:process.pid,childPid:child.pid});
}
void main().catch(error=>{console.error(error);process.exitCode=1;});
