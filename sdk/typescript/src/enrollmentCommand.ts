import { spawn } from 'node:child_process';
import { CodexEnrollmentError } from './codexAutojoin';
import type { CodexAgentClaim } from './codexBindings';

/** The installed operator command receives one public claim, with no shell,
 * bounded output and no retries. Only a typed pre-submission refusal is safe
 * to repeat; every post-spawn transport failure remains Unknown.
 */
export function runEnrollmentCommand(argv: string[], claim: CodexAgentClaim): Promise<unknown> {
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
        const result = JSON.parse(bytes.toString('utf8'));
        if(result && ['not_submitted','unknown'].includes(result.outcome) && typeof result.detail==='string') {
          throw new CodexEnrollmentError(result.outcome,result.detail);
        }
        if(code!==0)throw new Error(`Enrollment exited ${code}; recover the original operation`);
        resolve(result);
      } catch(error) {reject(error);}
    });
    child.stdin.end(JSON.stringify(claim)+'\n');
  });
}
