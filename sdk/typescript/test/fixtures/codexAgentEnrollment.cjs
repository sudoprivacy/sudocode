// Protocol-only operator adapter. No Node, issuer, key or credential is used.
const fs=require('node:fs');
const path=require('node:path');
const root=process.argv[2];
let input='';
process.stdin.on('data',chunk=>{input+=chunk;});
process.stdin.on('end',()=>{
  const claim=JSON.parse(input);
  const file=path.join(root,claim.actor+'.profile');
  fs.writeFileSync(file,JSON.stringify({
    scodeArgv:[process.execPath,path.join(__dirname,'scode-direct-a2a-agent.mjs')],
    workspace:claim.workspace,acpSessionId:'fixture-'+claim.operationId,closeTimeoutMs:300,
    env:{HYDRA_DIRECT_FIXTURE_JOURNAL:path.join(root,'acp.jsonl'),SCODE_DIRECT_FIXTURE_ACTOR:claim.actor},
  }),{flag:'wx',mode:0o600});
  fs.appendFileSync(path.join(root,'claims.jsonl'),JSON.stringify(claim)+'\n',{mode:0o600});
  process.stdout.write(JSON.stringify({actor:claim.actor,configFile:file})+'\n');
});
