// Test-only transparent relay around a REAL native stdio producer. Drop its
// actual queue receipt, prove its inventory stayed empty, then let it exit.
const fs=require('node:fs'),{spawn}=require('node:child_process');
const child=spawn(process.argv[2],process.argv.slice(3),{env:process.env,stdio:['pipe','pipe','pipe']});
const log=value=>fs.appendFileSync(process.env.NATIVE_QUEUE_RELAY_JOURNAL,JSON.stringify(value)+'\n');
log({phase:'spawned',pid:child.pid});
let input='',output='',queueId,checking=false;
const inspect='fixture-empty-after-lost-reply';
process.stdin.on('data',bytes=>{
 input+=bytes;let end;
 while((end=input.indexOf('\n'))>=0){const line=input.slice(0,end);input=input.slice(end+1);const call=JSON.parse(line);
  log({phase:'request',id:call.id,method:call.method,clientUserMessageId:call.params?.clientUserMessageId});
  if(call.method==='thread/queue/add')queueId=call.id;
  child.stdin.write(line+'\n');
 }
});
process.stdin.on('end',()=>child.stdin.end());process.stdin.on('error',()=>{});child.stdin.on('error',()=>{});
child.stdout.on('data',bytes=>{
 output+=bytes;let end;
 while((end=output.indexOf('\n'))>=0){const line=output.slice(0,end);output=output.slice(end+1);const response=JSON.parse(line);
  if(response.id===queueId&&response.result?.queuedSubmission){
   log({phase:'committed-receipt-dropped',pid:child.pid,result:response.result});checking=true;
   child.stdin.write(JSON.stringify({jsonrpc:'2.0',id:inspect,method:'thread/loaded/list',params:{}})+'\n');continue;
  }
  if(response.id===inspect){
   log({phase:'after-inventory',pid:child.pid,result:response.result,error:response.error});
   if(!response.result||response.result.data.length!==0||response.result.nextCursor!==null)process.exitCode=1;
   child.stdin.end();continue;
  }
  if(response.method==='thread/started'){log({phase:'unexpected-thread-loaded',response});process.exitCode=1;}
  if(!checking)process.stdout.write(line+'\n');
 }
});
child.stderr.pipe(process.stderr);
child.on('error',error=>{log({phase:'error',error:String(error)});process.exitCode=1;process.stdin.destroy();process.stdout.end();});
child.on('close',(code,signal)=>{log({phase:'exit',pid:child.pid,code,signal});process.exitCode||=code||signal?1:0;process.stdin.destroy();process.stdout.end();});
