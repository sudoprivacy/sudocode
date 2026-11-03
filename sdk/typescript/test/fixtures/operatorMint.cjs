// Public protocol fixture ONLY. No CA, Node credentials, signing or network.
const fs=require('node:fs');
const path=require('node:path');
const directory=process.argv[2];
let input='';
process.stdin.on('data',bytes=>{input+=bytes;});
process.stdin.on('end',()=>{
  const claim=JSON.parse(input);
  fs.appendFileSync(path.join(directory,'mint.jsonl'),JSON.stringify(claim)+'\n');
  if(process.argv[3]==='lost')process.exit(17);
  const credentialDirectory=path.join(directory,claim.actor);
  fs.mkdirSync(credentialDirectory);
  fs.writeFileSync(path.join(credentialDirectory,'credential.json'),JSON.stringify({version:1,agent:claim.actor}));
  process.stdout.write(JSON.stringify({version:1,actor:claim.actor,operationId:claim.operationId,credentialDirectory}));
});
