import fs from 'node:fs';
import net from 'node:net';
const args=process.argv.slice(2);
fs.appendFileSync(process.env.SCODE_LAUNCH_CALLS,JSON.stringify(args)+'\n');
if(args.slice(-3).join(' ')==='app-server daemon start'){
  console.log(JSON.stringify({status:'running',managedCodexVersion:'fixture'}));
}else if(args.slice(-2).join(' ')==='app-server proxy'){
  const socket=net.createConnection({host:'127.0.0.1',port:Number(process.env.SCODE_LAUNCH_PORT)});
  process.stdin.pipe(socket);socket.pipe(process.stdout);
  socket.on('close',()=>process.exit());
  socket.on('error',error=>{console.error(String(error));process.exitCode=1;});
}else{
  console.log(JSON.stringify({foregroundArgs:args}));
  process.exitCode=17; // The foreground entry must preserve native exit status.
}
