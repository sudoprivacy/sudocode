const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
const assert = require('node:assert/strict');
const { createRequire } = require('node:module');
const { createHash } = require('node:crypto');
const { execFileSync } = require('node:child_process');
const config = JSON.parse(fs.readFileSync(process.argv[2], 'utf8'));
const source = path.resolve(__dirname, '..');
const req = createRequire(path.join(source, 'package.json'));
assert.equal(createHash('sha256').update(fs.readFileSync(config.scode)).digest('hex'), config.scodeSha256);
config.sampleConfig = path.join(source, '../../rust/crates/runtime/src/sudocode.sample.json');
const { Client } = req('@modelcontextprotocol/sdk/client/index.js');
const { StdioClientTransport } = req('@modelcontextprotocol/sdk/client/stdio.js');
const sdk = require(path.join(config.install, 'node_modules/@sudocode/acp/dist/index.cjs'));
const entry = path.join(config.install, 'node_modules/@sudocode/acp/dist/server.cjs');
const verified = JSON.parse(execFileSync(process.execPath, [entry, '--verify'], { encoding: 'utf8' }));
assert.equal(verified.sourceCommit, config.packageSource);
const notices = []; const wire = []; let inference = 0; let discovery = 0;
const requests=[]; const servers=[];
function modelFor(actor) { const server=http.createServer((r,s) => {
  let body='';r.on('data',x=>body+=String(x));r.on('end',()=>{
    let parsed;try{parsed=JSON.parse(body);}catch{}
    requests.push({at:new Date().toISOString(),actor,url:r.url,method:r.method,bodyBytes:Buffer.byteLength(body),model:parsed?.model,keys:parsed?Object.keys(parsed):[]});
    if(r.method==='GET'){discovery++;s.writeHead(404);}else{inference++;s.writeHead(503);}
    s.end('{}');
  });
});servers.push(server);return server; }
const wait = async (f, text) => { const start = Date.now(); while (!f()) {
  if (Date.now()-start > 30000) throw new Error('Timeout: '+text);
  await new Promise(r=>setTimeout(r,20));
}};
let a,b,client,transport;
(async()=>{
  for (const actor of config.actors) {
    const server=modelFor(actor.name);await new Promise(r=>server.listen(0,'127.0.0.1',r));
    const url=`http://127.0.0.1:${server.address().port}`;
    let sample = fs.readFileSync(config.sampleConfig,'utf8');
    sample = sample.replaceAll('https://api.anthropic.com',url).replaceAll('<YOUR_ANTHROPIC_API_KEY>','isolated-no-inference-key');
    fs.writeFileSync(path.join(actor.configHome,'sudocode.json'),sample);
  }
  const acp = actor => ({scodeArgv:[config.scode,'--auth','api-key','--model','sonnet',
      '--permission-mode','workspace-write','--allowedTools','agent_list','--allowedTools','send','acp'],
    workspace:actor.workspace, env:{...actor.env}, stderrLog:path.join(config.root,actor.name+'.stderr.log'),
    startupTimeoutMs:30000, closeTimeoutMs:2000,
    onProtocol:(direction,message)=>wire.push({actor:actor.name,direction,message})});
  a=await sdk.connectScode(acp(config.actors[0]));
  const sid=a.sessionId; await a.close(); a=undefined;
  const runtime = {...acp(config.actors[0]),acpSessionId:sid,version:1,
    auditLog:path.join(config.root,'mcp-audit.jsonl'), protocolLog:path.join(config.root,'mcp-protocol.jsonl')};
  delete runtime.onProtocol;
  const privateConfig=path.join(config.root,'mcp.json');fs.writeFileSync(privateConfig,JSON.stringify(runtime));
  client=new Client({name:'root-real-native-pair',version:'1'});
  transport=new StdioClientTransport({command:process.execPath,args:[entry,'--config',privateConfig],stderr:'pipe'});
  let stderr='';transport.stderr?.on('data',x=>{stderr+=String(x);});
  await client.connect(transport);
  const status=await client.callTool({name:'scode_status',arguments:{}});
  const nativeStatus=JSON.parse(status.content[0].text);
  assert.equal(nativeStatus.acpSessionId,sid);assert.equal(nativeStatus.directA2a.version,1);
  const message='ROOT_NATIVE_NODE_PAIR \u96f6\u6a21\u578b \u{1f9ea}\nexact\0plain text '+config.marker;
  const sent=await client.callTool({name:'scode_send',arguments:{to:config.actors[1].name,message,summary:'Isolated native pair acceptance'}});
  assert.equal(sent.isError,false,JSON.stringify(sent));
  const audit=sent._meta.sudocode.directA2a.audit;
  assert.equal(audit.actor,config.actors[0].name);assert.equal(audit.sessionId,sid);
  assert.match(audit.commandId,/^external-a2a-/);
  const agents=await client.callTool({name:'scode_agent_list',arguments:{activeOnly:false}});
  assert.equal(agents.isError,false,JSON.stringify(agents));
  assert.ok(agents.content[0].text.includes(config.actors[1].name));
  const forged=await client.callTool({name:'scode_send',arguments:{to:config.actors[1].name,message:'must-not-append',summary:'invalid sender',sender:'forged'}});
  assert.equal(forged.isError,true);
  assert.equal(inference,0,'direct send/list must make zero inference requests while the recipient is offline');
  const directInferenceRequests=inference;
  const calls=fs.readFileSync(path.join(config.root,'mcp-protocol.jsonl'),'utf8').trim().split('\n').map(JSON.parse).filter(x=>x.direction==='outgoing');
  assert.equal(calls.filter(x=>x.message?.method==='_sudocode/a2a/send').length,1);
  assert.equal(calls.filter(x=>x.message?.method==='session/prompt').length,0);
  // Start the recipient for the first time only after the direct-operation gate.
  // Its ordinary response model is a distinct later activity, never a send translator.
  b=await sdk.connectScode(acp(config.actors[1]),n=>notices.push(n));
  await wait(()=>notices.some(n=>JSON.stringify(n).includes(config.marker)),'real Node delivery to recipient');
  const delivered=notices.filter(n=>JSON.stringify(n).includes(config.marker));
  assert.equal(delivered.length,1,JSON.stringify(delivered));
  assert.ok(JSON.stringify(delivered[0]).includes(config.actors[0].name));
  assert.ok(delivered[0].update.content.text.includes(message), 'the receiver reads the exact durably appended message');
  assert.equal(requests.filter(x=>x.actor===config.actors[0].name&&x.method!=='GET').length,0,'sender never infers');
  const receipt={success:true,source:config.source,packageSource:config.packageSource,
    scodeSha256:config.scodeSha256,daemon:config.daemonReceipt,actualMcp:true,realTlsNode:true,
    sameSidReload:sid,actor:config.actors[0].name,recipient:config.actors[1].name,
    directInferenceRequests,recipientResponseInferenceRequests:requests.filter(x=>x.actor===config.actors[1].name&&x.method!=='GET').length,discoveryRequests:discovery,received:delivered,modelRequests:requests,
    sent,agents,forged,childStatus:nativeStatus,
    limits:['not a true idle Codex callback test','not in-flight pause/loss acceptance','not a fleet migration']};
  fs.writeFileSync(path.join(config.root,'native-sdk.receipt.json'),JSON.stringify(receipt,null,2)+'\n');
  console.log(JSON.stringify({success:true,actor:receipt.actor,recipient:receipt.recipient,directInferenceRequests,realTlsNode:true,sameSidReload:sid}));
})().catch(e=>{console.error(e);process.exitCode=1;}).finally(async()=>{
  await client?.close();await a?.close();await b?.close();for(const server of servers)server.close();
  fs.writeFileSync(path.join(config.root,'model-request-origins.json'),JSON.stringify({requests,inference,discovery},null,2));
  fs.writeFileSync(path.join(config.root,'recipient-notifications.json'),JSON.stringify(notices,null,2));
  fs.writeFileSync(path.join(config.root,'bootstrap-wire.json'),JSON.stringify(wire,null,2));
});
