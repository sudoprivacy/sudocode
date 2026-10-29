import { spawn } from 'node:child_process';
import { createHash, timingSafeEqual, randomBytes } from 'node:crypto';
import { appendFileSync, readFileSync, mkdirSync, statSync, writeFileSync } from 'node:fs';
import { connect, createServer, type Socket } from 'node:net';
import * as path from 'node:path';
import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import { connectCodexAutojoin, CodexEnrollmentError, type CodexAgentClaim, type CodexAgentProfile, type CodexAutojoin } from './codexAutojoin';
import { validateCodexConnectionConfig, type CodexConnectionConfig } from './codexConnection';
import { registerScodeMcpTools } from './mcpTools';
import { timeout } from './timeout';
import { ScodeA2aError } from './scodeA2a';

export interface CodexMcpConfig {
  version: 1;
  mode: 'codex-autojoin';
  native: CodexConnectionConfig;
  stateDirectory: string;
  auditLog: string;
  peers: string[];
  /** Explicit operator-owned adapter to the existing authorized enrollment path. */
  enrollmentArgv: string[];
  endpoint: {port: number; token: string};
}

/** Prepare the portable private config and the native global-MCP command.
 * Deliberately do not change a running controller, node or global configuration.
 */
export function prepareCodexMcp(file: string, nativeExecutable: string, enrollmentArgv: string[], peers: string[],
  entry: string, port = 23262): {configFile: string; registrationArgv: string[]; installed: false} {
  if(![file,nativeExecutable,entry].every(value=>typeof value==='string'&&path.isAbsolute(value))
      || !statSync(nativeExecutable).isFile() || !statSync(entry).isFile())throw new Error('Existing absolute native executable/entry and private config paths are required');
  const parent = path.dirname(file);
  mkdirSync(parent,{recursive:true,mode:0o700});
  const config: CodexMcpConfig = {version:1,mode:'codex-autojoin',native:{codexArgv:[nativeExecutable]},
    stateDirectory:path.join(parent,'codex-agents'),auditLog:path.join(parent,'codex-network.jsonl'),
    peers,enrollmentArgv,endpoint:{port,token:randomBytes(32).toString('hex')}};
  // Validate before publishing. This initializer neither runs the enrollment
  // command nor copies its authority into the runtime package.
  validateConfig(config);
  writeFileSync(file,JSON.stringify(config,null,2)+'\n',{flag:'wx',mode:0o600});
  return {configFile:file,registrationArgv:[nativeExecutable,'mcp','add','nexus_scode','--',process.execPath,entry,
    '--codex','--config',file],installed:false};
}

function validateConfig(config: CodexMcpConfig): void {
  if (config?.version !== 1 || config.mode !== 'codex-autojoin'
      || !path.isAbsolute(config.stateDirectory) || !path.isAbsolute(config.auditLog)
      || !Array.isArray(config.enrollmentArgv) || !config.enrollmentArgv.length
      || config.enrollmentArgv.some(arg => typeof arg !== 'string' || !arg || arg.includes('\0'))
      || !path.isAbsolute(config.enrollmentArgv[0])
      || !Array.isArray(config.peers) || config.peers.some(peer=>typeof peer!=='string'||!peer)
      || !Number.isInteger(config.endpoint?.port) || config.endpoint.port < 1024 || config.endpoint.port > 65535
      || !/^[0-9a-f]{64}$/.test(config.endpoint.token)) throw new Error('Invalid Codex autojoin MCP configuration');
  validateCodexConnectionConfig(config.native);
}

function readConfig(file: string): {config: CodexMcpConfig; sha256: string} {
  if (!path.isAbsolute(file)) throw new Error('An absolute private Codex MCP config is required');
  const bytes = readFileSync(file);
  const config = JSON.parse(bytes.toString('utf8')) as CodexMcpConfig;
  validateConfig(config);
  return {config,sha256:createHash('sha256').update(bytes).digest('hex')};
}

function equal(a: unknown, b: string): boolean {
  return typeof a === 'string' && Buffer.byteLength(a) === Buffer.byteLength(b)
    && timingSafeEqual(Buffer.from(a),Buffer.from(b));
}

/** Exactly one bounded header precedes the standard MCP newline stream. */
function readHeader(socket: Socket): Promise<unknown> {
  return new Promise((resolve,reject) => {
    let buffer = Buffer.alloc(0);
    const cleanup = () => {socket.removeListener('data',data);socket.removeListener('error',error);socket.removeListener('end',end);};
    const error = (error: Error) => {cleanup();reject(error);};
    const end = () => error(new Error('Local bridge closed before authentication'));
    const data = (bytes: Buffer) => {
      buffer = Buffer.concat([buffer,bytes]);
      const newline = buffer.indexOf(10);
      if (buffer.length > 4096 && newline < 0) {error(new Error('Oversized local authentication header'));return;}
      if (newline < 0) return;
      cleanup();socket.pause();
      try {
        if (newline > 4096) throw new Error('Oversized local authentication header');
        const result: unknown = JSON.parse(buffer.subarray(0,newline).toString('utf8'));
        socket.unshift(buffer.subarray(newline+1));resolve(result);
      } catch(e) {reject(e);}
    };
    socket.on('data',data);socket.once('error',error);socket.once('end',end);socket.resume();
  });
}

function enroll(argv: string[], claim: CodexAgentClaim): Promise<CodexAgentProfile> {
  return new Promise((resolve,reject) => {
    const child = spawn(argv[0],argv.slice(1),{stdio:['pipe','pipe','ignore'],shell:false,windowsHide:true});
    let bytes = Buffer.alloc(0);
    let failure: Error | undefined;
    const kill = (error: Error) => {failure ??= error;child.kill('SIGKILL');};
    const timer = setTimeout(()=>kill(new Error('Enrollment response timed out; recover the original operation')),30000);
    child.on('error',error=>{failure ??= child.pid===undefined
      ? new CodexEnrollmentError('not_submitted',`Enrollment adapter did not start: ${error.message}`) : error;});
    child.stdin.on('error',error=>{failure ??= error;});
    child.stdout.on('data',(chunk: Buffer)=>{
      bytes = Buffer.concat([bytes,chunk]);
      if(bytes.length>8192)kill(new Error('Enrollment response exceeded its bound'));
    });
    child.once('close',code=>{
      clearTimeout(timer);
      try {
        if(failure)throw failure;
        const profile = JSON.parse(bytes.toString('utf8')) as CodexAgentProfile & {outcome?: unknown;detail?: unknown};
        if(profile && ['not_submitted','unknown'].includes(String(profile.outcome)) && typeof profile.detail==='string'){
          throw new CodexEnrollmentError(profile.outcome as 'not_submitted'|'unknown',profile.detail);
        }
        if(code!==0)throw new Error(`Enrollment exited ${code}; recover the original operation`);
        if(!profile || Object.keys(profile).some(key=>!['actor','configFile'].includes(key))) throw new Error('Invalid enrollment response');
        resolve(profile);
      } catch(error) {reject(error);}
    });
    child.stdin.end(JSON.stringify(claim)+'\n');
  });
}

/** One per-user owner, elected by an exclusive loopback bind. It owns the native
 * observer and all Agent receivers; stdio clients only forward the MCP stream.
 */
export async function runCodexMcpOwner(file: string): Promise<void> {
  const {config,sha256} = readConfig(file);
  const audit = (entry: unknown) => appendFileSync(config.auditLog,JSON.stringify({
    at:new Date().toISOString(),provenance:'codex-autojoin-owner',...entry as object,
  })+'\n',{mode:0o600});
  // Audit failure must prevent opening the shared command endpoint.
  audit({phase:'starting',pid:process.pid});
  let network: CodexAutojoin | undefined;
  let idleTimer: ReturnType<typeof setTimeout> | undefined;
  let stopping: Promise<void> | undefined;
  const sockets = new Set<Socket>();
  let finish = () => {};
  const finished = new Promise<void>(resolve=>{finish=resolve;});
  const stop = () => stopping ??= (async()=>{
    clearTimeout(idleTimer);
    listener.close();
    for(const socket of sockets)socket.destroy();
    await network?.close();
  })().finally(finish);
  const scheduleExit = () => {
    clearTimeout(idleTimer);
    if(!sockets.size)idleTimer=setTimeout(()=>{void stop();},5000);
  };
  const listener = createServer(socket=>{
    sockets.add(socket);clearTimeout(idleTimer);
    socket.on('error',()=>{});
    socket.once('close',()=>{sockets.delete(socket);scheduleExit();});
    void (async()=>{
      const hello = await timeout(readHeader(socket),3000,'Local MCP authentication') as {token?: unknown;configSha256?: unknown};
      if(!equal(hello?.token,config.endpoint.token) || !equal(hello?.configSha256,sha256)) {
        throw new Error('Local MCP owner configuration/authentication mismatch');
      }
      const server = new McpServer({name:'nexus-scode',version:'1.0.0'});
      registerScodeMcpTools(server,async extra=>{
        if(!network)throw new ScodeA2aError('Native inventory is not ready; no A2A command was submitted','not_submitted');
        // A resumed root may precede the periodic poll. Finish its attachment
        // before tool admission; this never retries an actual task or send.
        await network.refresh();
        return {connection:network.connectionFor(extra._meta?.threadId),nativeThreadId:extra._meta?.threadId as string,
          auditLog:config.auditLog};
      },undefined,()=>({mode:'codex-autojoin',ownerPid:process.pid,agents:network?.status()??[],
        state:network?'observing':'connecting',frontendPresence:'not inferred; ownership follows loaded native roots'}));
      socket.write(JSON.stringify({ready:true})+'\n');
      await server.connect(new StdioServerTransport(socket,socket));
      socket.resume();
    })().catch(()=>socket.destroy());
  });
  await new Promise<void>((resolve,reject)=>{
    listener.once('error',reject);listener.listen(config.endpoint.port,'127.0.0.1',resolve);
  });
  audit({phase:'listening',pid:process.pid});
  const onSignal = () => {void stop();};
  process.once('SIGTERM',onSignal);process.once('SIGINT',onSignal);
  scheduleExit();
  try {
    network = await connectCodexAutojoin({native:config.native,stateDirectory:config.stateDirectory,peers:config.peers,
      resolveAgent:claim=>enroll(config.enrollmentArgv,claim),onError:error=>{
        try {audit({phase:'attachment_error',error:String(error)});} catch { /* No command submission. */ }
      }});
    if(stopping)await network.close();
    await finished;
  } finally {
    await stop();
    process.removeListener('SIGTERM',onSignal);process.removeListener('SIGINT',onSignal);
  }
}

async function dial(port: number): Promise<Socket> {
  return new Promise((resolve,reject)=>{
    const socket=connect(port,'127.0.0.1');
    socket.once('connect',()=>{socket.removeListener('error',reject);resolve(socket);});
    socket.once('error',reject);
  });
}

/** A global Codex MCP entry. Startup performs no inference and never infers a
 * thread from cwd/env. Host tool-call _meta is passed through byte for byte.
 */
export async function runCodexMcp(file: string, entry: string): Promise<void> {
  const {config,sha256} = readConfig(file);
  let socket: Socket;
  try {socket=await dial(config.endpoint.port);}
  catch(error) {
    if((error as NodeJS.ErrnoException).code!=='ECONNREFUSED')throw error;
    const child=spawn(process.execPath,[entry,'--config',file,'--codex-owner'],{
      stdio:'ignore',detached:true,windowsHide:true,shell:false,
    });
    child.on('error',()=>{});child.unref();
    const end=Date.now()+5000;
    for(;;){
      try {socket=await dial(config.endpoint.port);break;}
      catch(e){
        if((e as NodeJS.ErrnoException).code!=='ECONNREFUSED' || Date.now()>end)throw e;
        await new Promise(resolve=>setTimeout(resolve,50));
      }
    }
  }
  socket.on('error',()=>{});
  try {
    const welcome=readHeader(socket);
    socket.write(JSON.stringify({token:config.endpoint.token,configSha256:sha256})+'\n');
    if((await timeout(welcome,3000,'Local MCP owner handshake') as {ready?:unknown})?.ready!==true)throw new Error('Invalid local MCP owner');
    process.stdin.pipe(socket);socket.pipe(process.stdout);socket.resume();
    await new Promise<void>((resolve,reject)=>{
      socket.once('close',()=>resolve());socket.once('error',reject);
      process.stdin.once('end',()=>socket.end());
    });
  } finally {socket.destroy();}
}
