import { appendFileSync } from 'node:fs';
import { createHash, randomUUID } from 'node:crypto';
import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import type { CallToolResult } from '@modelcontextprotocol/sdk/types.js';
import { z } from 'zod';
import type { ScodeConnection } from './scodeAcp';
import { listScodeAgents, sendScodeMessage, scodeNativeA2aAudit, supportsScodeDirectA2a, ScodeA2aError, type ScodeA2aResult } from './scodeA2a';

export interface McpAgentBinding {
  connection: ScodeConnection;
  auditLog: string;
  nativeThreadId?: string;
}
type Extra = {requestId: string | number; _meta?: Record<string, unknown>};
function digest(value: unknown): {sha256: string; bytes: number} {
  const bytes = Buffer.from(JSON.stringify(value));
  return {sha256:createHash('sha256').update(bytes).digest('hex'),bytes:bytes.length};
}
function errorResult(error: unknown) {
  return {isError:true,content:[{type:'text' as const,text:JSON.stringify({
    outcome:error instanceof ScodeA2aError ? error.outcome : 'unknown',error:String(error),
    rpcError:error instanceof ScodeA2aError ? error.rpcError : undefined,automaticRetry:false,
  })}]};
}

/** Both fixed-session and native-root MCP hosts use these exact tools/audit. */
export function registerScodeMcpTools(server: McpServer,
  select: (extra: Extra) => McpAgentBinding | Promise<McpAgentBinding>,
  prompt?: (connection: ScodeConnection, text: string) => Promise<CallToolResult>,
  status?: () => unknown,
): void {
  const withBinding = async (extra: Extra, action: (binding: McpAgentBinding) => Promise<CallToolResult>): Promise<CallToolResult> => {
    try { return await action(await select(extra)); } catch (error) { return errorResult(error); }
  };
  const external = async (method: string, input: unknown, extra: Extra,
    action: (connection: ScodeConnection) => Promise<ScodeA2aResult>) => withBinding(extra, async binding => {
    const {connection} = binding;
    const base = {id:randomUUID(),requestId:extra.requestId,method,sessionId:connection.sessionId,
      pid:connection.pid,nativeThreadId:binding.nativeThreadId};
    const audit = (entry: unknown) => appendFileSync(binding.auditLog,JSON.stringify({
      at:new Date().toISOString(),provenance:'external-command',...base,...entry as object,
    })+'\n',{mode:0o600});
    try { audit({phase:'requested',input:digest(input)}); }
    catch(error) { return errorResult(new ScodeA2aError(`Cannot record external command: ${String(error)}`,'not_submitted')); }
    try {
      const result = await action(connection);
      try { audit({phase:'returned',isError:result.isError,output:digest(result),nativeAudit:scodeNativeA2aAudit(result)}); }
      catch(error) { process.stderr.write(`External command ${base.id} result audit failed: ${String(error)}\n`); }
      return {content:[{type:'text' as const,text:result.content}],isError:result.isError,
        ...(result._meta===undefined?{}:{_meta:result._meta as Record<string,unknown>})};
    } catch(error) {
      const result = errorResult(error);
      try { audit({phase:'error',...JSON.parse(result.content[0].text)}); }
      catch(auditError) { process.stderr.write(`External command ${base.id} error audit failed: ${String(auditError)}\n`); }
      return result;
    }
  });
  server.registerTool('scode_prompt',{
    description:'Give a task to the owned full scode session. This may use its model. Use scode_send for explicit messages.',
    inputSchema:{text:z.string().min(1)},
  },async ({text},extra) => withBinding(extra,async ({connection}) => prompt
    ? prompt(connection,text)
    : {content:[{type:'text',text:JSON.stringify(await connection.request('session/prompt',{
      sessionId:connection.sessionId,prompt:[{type:'text',text}],
    }))}]} ));
  server.registerTool('scode_send',{
    description:'Send exact plain text through the owned scode without model inference. Uses native send policy/hooks. No automatic retry; a transport error after submission is Unknown.',
    inputSchema:z.object({to:z.string().min(1),message:z.string(),summary:z.string().min(1)}).strict(),
  },async (input,extra) => external('send',input,extra,connection=>sendScodeMessage(connection,input)));
  server.registerTool('scode_agent_list',{
    description:'Read the owned scode agent directory without model inference. Visibility does not prove a peer is online.',
    inputSchema:z.object({activeOnly:z.boolean().optional()}).strict(),
  },async (input,extra) => external('agent_list',input,extra,connection=>listScodeAgents(connection,input)));
  server.registerTool('scode_status',{
    description:'Report bridge ownership and attachment state. Directory visibility does not establish connectivity.',inputSchema:{},
  },async (_,extra) => status ? {content:[{type:'text',text:JSON.stringify(status())}]}
    : await withBinding(extra,async ({connection})=>({content:[{type:'text',text:JSON.stringify({
      pid:connection.pid,acpSessionId:connection.sessionId,closed:connection.closed,
      directA2a:supportsScodeDirectA2a(connection)?{version:1}:null,notifications:'connection-lifetime',
    })}]})));
}
