import fs from 'node:fs';
import path from 'node:path';
const args=process.argv.slice(2);
const root=process.env.SCODE_SETUP_FAKE_ROOT;
fs.appendFileSync(path.join(root,'calls.jsonl'),JSON.stringify(args)+'\n');
const selected=path.join(root,'registration.json');
if(args.join(' ')==='mcp get nexus_scode --json'){
  if(fs.existsSync(selected))process.stdout.write(fs.readFileSync(selected));
  else{console.error("Error: No MCP server named 'nexus_scode' found.");process.exitCode=1;}
}else if(args.slice(0,4).join(' ')==='mcp add nexus_scode --'){
  fs.writeFileSync(selected,JSON.stringify({name:'nexus_scode',enabled:true,
    transport:{type:'stdio',command:args[4],args:args.slice(5),env:null,env_vars:[],cwd:null}}));
  if(process.env.SCODE_SETUP_REPLACE_ENTRY)fs.writeFileSync(process.env.SCODE_SETUP_REPLACE_ENTRY,'independent edit\n');
  if(process.env.SCODE_SETUP_LOST_REGISTRATION){console.error('fixture lost acknowledgement after persistence');process.exitCode=1;}
}else if(args.join(' ')==='app-server daemon start'){
  console.log(JSON.stringify({status:'running',managedCodexVersion:'fixture'}));
}else{
  console.log(JSON.stringify({foregroundArgs:args}));process.exitCode=17;
}
