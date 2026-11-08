// Native stdio protocol fixture only; actual native proof uses nativeCodexQueue.cjs.
const fs=require('node:fs'),crypto=require('node:crypto'),readline=require('node:readline');
const scenario=process.argv[2]??'normal';
const journal=process.env.NATIVE_QUEUE_FIXTURE_JOURNAL;
const log=row=>fs.appendFileSync(journal,JSON.stringify({pid:process.pid,...row})+'\n');
let inventory=0;
const send=value=>process.stdout.write(JSON.stringify(value)+'\n');
if(scenario==='ignore-eof')process.on('SIGTERM',()=>{});
readline.createInterface({input:process.stdin}).on('line',line=>{
 const call=JSON.parse(line);log(call);if(call.id===undefined)return;
 if(scenario==='server-request'&&call.method==='initialize'){
  send({id:call.id,method:'account/chatgptAuthTokens/refresh',params:{}});return;
 }
 let result={};
 if(call.method==='thread/loaded/list'){
  inventory++;result={data:scenario==='loaded-before'||(scenario==='loaded-after'&&inventory===2)?['other-root']:[],nextCursor:null};
 }
 if(call.method==='thread/queue/add'){
  log({phase:'append',params:call.params});
  if(scenario==='loss'){process.exit(17);}
  if(scenario==='post-error'){send({id:call.id,error:{code:-32602,message:'backend failure after possible append'}});return;}
  result={queuedSubmission:{id:crypto.randomUUID(),clientUserMessageId:call.params.clientUserMessageId,input:call.params.input}};
 }
 send({id:call.id,result});
 if(scenario==='late-load'&&call.method==='thread/loaded/list'&&inventory===2){send({method:'thread/started',params:{thread:{id:'other-root'}}});}
}).on('close',()=>{
 if(scenario==='ignore-eof'){setInterval(()=>{},1000);return;}
 process.exitCode=0;
});
