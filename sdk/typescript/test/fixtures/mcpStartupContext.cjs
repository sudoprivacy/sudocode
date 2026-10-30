// Passive real-native startup probe. Never read credentials or start a model.
const fs = require('node:fs');
const path = require('node:path');
const readline = require('node:readline');
const output = process.env.CODEX_MCP_CONTEXT_OUTPUT;
if (!output || !path.isAbsolute(output)) throw new Error('Private fixture output directory required');
const receipt = {pid:process.pid,cwd:process.cwd(),threadId:process.env.CODEX_THREAD_ID??null,initialize:null};
const lines = readline.createInterface({input:process.stdin});
lines.on('line', line=>{
  const request=JSON.parse(line);
  if(request.method==='initialize'){
    receipt.initialize=request.params;
    fs.writeFileSync(path.join(output,`mcp-context-${process.pid}.json`),JSON.stringify(receipt,null,2)+'\n',{mode:0o600,flag:'wx'});
  }
  if(request.id===undefined)return;
  const result=request.method==='initialize'?{
    protocolVersion:request.params.protocolVersion,capabilities:{tools:{}},serverInfo:{name:'context-probe',version:'1'},
  }:request.method==='tools/list'?{tools:[]}:{};
  process.stdout.write(JSON.stringify({jsonrpc:'2.0',id:request.id,result})+'\n');
});
