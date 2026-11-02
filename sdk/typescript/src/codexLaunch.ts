import { spawn } from 'node:child_process';
import * as path from 'node:path';
import { connectCodex, validateCodexConnectionConfig, type CodexConnectionConfig } from './codexConnection';

// Unknown options are delegated unchanged, rather than guessing whether their
// next word is an option value or a prompt.
const values = new Set(['-a','--ask-for-approval','-s','--sandbox','-m','--model',
  '-C','--cd','-i','--image','--add-dir','--local-provider','--enable','--disable']);
const flags = new Set(['--search','--no-alt-screen',
  '--dangerously-bypass-approvals-and-sandbox']);
const noninteractive = new Set(['exec','e','review','login','logout','mcp','plugin',
  'app-server','remote-control','app','completion','update','doctor','sandbox','debug',
  'apply','a','queue','archive','delete','migrate-rollouts','migrate-nfs-db','unarchive',
  'cloud','exec-server','features','help','agents']);

function redundantReviewer(args: readonly string[]): { args: string[]; cwd: string } | undefined {
  const remove = new Set<number>();
  let cwd = process.cwd();
  let command = false;
  for (let i=0;i<args.length;i++) {
    const arg = args[i];
    if (arg==='--') break; // Prompts may literally contain "-c" afterwards.
    if (['-h','--help','-V','--version','--no-daemon','--remote','--profile','-p',
      '--strict-config','--dangerously-bypass-hook-trust','--oss','--approve-for-me','--worktree'].includes(arg)
      || arg.startsWith('--remote=') || arg.startsWith('--profile=')) return;
    let config: string | undefined;
    let separate = false;
    if (arg==='-c' || arg==='--config') {config=args[i+1];separate=true;}
    else if (arg.startsWith('--config=')) config=arg.slice('--config='.length);
    else if (arg.startsWith('-c') && arg.length>2) config=arg.slice(2);
    if (config!==undefined) {
      // Other overrides, including a different reviewer, keep native semantics.
      if (!/^\s*approvals_reviewer\s*=\s*(?:user|"user"|'user')\s*$/.test(config)) return;
      remove.add(i);if(separate)remove.add(++i);
    } else if (values.has(arg)) {
      const value=args[++i];if(value===undefined)return;
      if(arg==='-C'||arg==='--cd')cwd=path.resolve(cwd,value);
    } else if (arg.includes('=') && values.has(arg.split('=')[0])) {
      if(arg.startsWith('--cd='))cwd=path.resolve(cwd,arg.slice('--cd='.length));
    } else if (arg.startsWith('-')) {
      if(!flags.has(arg))return;
    } else if (!command) {
      if(noninteractive.has(arg))return;
      if(arg!=='resume'&&arg!=='fork')break;
      command=true;
    }
  }
  return remove.size?{args:args.filter((_,i)=>!remove.has(i)),cwd}:undefined;
}

function foreground(argv: string[], env?: Record<string,string>, capture=false): Promise<{code:number;stdout:string}> {
  return new Promise((resolve,reject)=>{
    const child=spawn(argv[0],argv.slice(1),{shell:false,env,windowsHide:capture,
      stdio:capture?['ignore','pipe','pipe']:'inherit'});
    let stdout='',stderr='';
    child.stdout?.on('data',bytes=>{stdout+=String(bytes);});
    child.stderr?.on('data',bytes=>{stderr+=String(bytes);});
    const signals={SIGINT:()=>child.kill('SIGINT'),SIGTERM:()=>child.kill('SIGTERM')};
    for(const [signal,forward] of Object.entries(signals))process.on(signal,forward);
    const cleanup=()=>{for(const [signal,forward] of Object.entries(signals))process.removeListener(signal,forward);};
    child.once('error',error=>{cleanup();reject(error);});
    child.once('close',(code,signal)=>{
      cleanup();
      if(capture && (code!==0 || signal))reject(new Error(`Native Codex preparation failed (${code}/${signal}): ${stderr}`));
      else resolve({code:code??(signal==='SIGINT'?130:143),stdout});
    });
  });
}

/** Foreground compatibility only. Native Codex still owns its daemon and TUI.
 * Never change a reviewer, enroll an actor, install a package or start Nexus.
 */
export async function runCodexUserEntry(native: CodexConnectionConfig, args: string[]): Promise<number> {
  validateCodexConnectionConfig(native);
  if(args.some(arg=>typeof arg!=='string'||arg.includes('\0')))throw new Error('Invalid native Codex arguments');
  // Official interpreter/wrapper arguments precede native CLI options.
  const prefix=native.codexArgv.slice(1);
  const firstOption=prefix.findIndex(arg=>arg.startsWith('-'));
  const wrapper=firstOption<0?prefix:prefix.slice(0,firstOption);
  const original=[...(firstOption<0?[]:prefix.slice(firstOption)),...args];
  const normalized=redundantReviewer(original);
  let selected=original;
  if(normalized){
    // The official cold-start operation preserves an existing package choice.
    // This entry implements no native package-selection or upgrade lifecycle.
    await foreground([...native.codexArgv,'app-server','daemon','start'],native.env,true);
    const connection=await connectCodex(native);
    try{
      const result=await connection.request<{config:{approvals_reviewer?:unknown}}>('config/read',
        {cwd:normalized.cwd,includeLayers:false});
      if(result.config?.approvals_reviewer!=='user'){
        throw new Error('Effective Codex reviewer is not user; refusing to remove the explicit reviewer override. Configure user review through Codex before using this shared entry.');
      }
    }finally{await connection.close();}
    selected=normalized.args;
    console.error('Codex A2A: user review is already configured; using the shared background server.');
  }
  return (await foreground([native.codexArgv[0],...wrapper,...selected],native.env)).code;
}
