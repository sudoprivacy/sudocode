// Public protocol fixture ONLY. No CA, Node credentials, signing or network.
// It keeps the Node operator's records: an attempt is reserved before the
// request leaves, and a receipt once it is issued. A reserved operation replays
// its receipt; one reserved without a receipt is Unknown and never resubmitted.
const fs=require('node:fs');
const path=require('node:path');
const directory=process.argv[2];
let input='';
process.stdin.on('data',bytes=>{input+=bytes;});
process.stdin.on('end',()=>{
  const claim=JSON.parse(input);
  const operations=path.join(directory,'operations');
  fs.mkdirSync(operations,{recursive:true});
  const attempt=path.join(operations,claim.actor+'.attempt'), receipt=path.join(operations,claim.actor+'.receipt');
  if(fs.existsSync(attempt)){
    if(fs.existsSync(receipt)){process.stdout.write(fs.readFileSync(receipt,'utf8'));return;}
    process.stdout.write(JSON.stringify({outcome:'unknown',detail:'original issuance may have committed; no RPC retry'}));
    return;
  }
  if(process.argv[3]==='outage'&&fs.existsSync(path.join(directory,'outage'))){
    process.stdout.write(JSON.stringify({outcome:'not_submitted',detail:'MintAgent connect failed'}));
    return;
  }
  fs.writeFileSync(attempt,JSON.stringify(claim),{flag:'wx'});
  fs.appendFileSync(path.join(directory,'mint.jsonl'),JSON.stringify(claim)+'\n');
  if(process.argv[3]==='lost')process.exit(17);
  const credentialDirectory=path.join(directory,claim.actor);
  fs.mkdirSync(credentialDirectory);
  fs.writeFileSync(path.join(credentialDirectory,'credential.json'),JSON.stringify({version:1,agent:claim.actor}));
  const issued=JSON.stringify({version:1,actor:claim.actor,operationId:claim.operationId,credentialDirectory});
  fs.writeFileSync(receipt,issued,{flag:'wx'});
  process.stdout.write(issued);
});
