import * as acp from '@agentclientprotocol/sdk';
import { appendFileSync, readFileSync, existsSync, writeFileSync } from 'node:fs';
import { spawn } from 'node:child_process';
import * as path from 'node:path';
import { Readable, Writable } from 'node:stream';
import { createHash, randomUUID } from 'node:crypto';

// Protocol fixture only. Real engine/policy/mailbox and zero-inference proofs
// belong to sudocode's stdio acceptance, not this Hydra transport smoke.
const scenario = process.argv[2] ?? 'normal';
const actor = scenario === 'operator'
  ? JSON.parse(readFileSync(path.join(process.cwd(),'.nexus/sudocode/settings.json'),'utf8')).agentName
  : process.env.SCODE_DIRECT_FIXTURE_ACTOR ?? 'bound-fixture-actor';
const journal = process.env.HYDRA_DIRECT_FIXTURE_JOURNAL;
const log = entry => appendFileSync(journal, JSON.stringify({ pid: process.pid, ...entry })+'\n');
let sessionId;
let peerOwned = 0;
let returnedInitialListFailure = false;
const frame = (hash, value) => {
  const bytes = Buffer.from(value), length = Buffer.alloc(8);
  length.writeBigUInt64BE(BigInt(bytes.length)); hash.update(length).update(bytes);
};
const inputDigest = (tool, params) => {
  const input = tool === 'send' ? { to: params.to, message: params.message,
    ...(params.summary === undefined ? {} : { summary: params.summary }) }
    : params.activeOnly === undefined ? {} : { active_only: params.activeOnly };
  const hash = createHash('sha256').update('sudocode.direct-a2a.input.v1\0');
  frame(hash, tool);
  const keys = Object.keys(input).sort(), count = Buffer.alloc(8); count.writeBigUInt64BE(BigInt(keys.length)); hash.update(count);
  for (const key of keys) {
    frame(hash, key);
    const value = input[key]; hash.update(Buffer.from([typeof value === 'string' ? 1 : value ? 3 : 2]));
    if (typeof value === 'string') frame(hash, value);
  }
  return hash.digest('hex');
};
const result = (value, tool, input, operationError = false) => {
  const content = operationError ? value : JSON.stringify(value);
  const hash = createHash('sha256').update('sudocode.direct-a2a.result.v1\0').update(Buffer.from([operationError ? 1 : 0])); frame(hash, content);
  return { content, isError: operationError || scenario === 'flipped-is-error', _meta: { sudocode: { directA2a: { audit: {
    version: 1, commandId: `external-a2a-${randomUUID()}`, sessionId: scenario === 'bad-audit' ? 'wrong' : sessionId,
    actor, namespaceRoot: '', tool,
    requestedInputSha256: inputDigest(tool, input), effectiveInputSha256: inputDigest(tool, input), resultSha256: hash.digest('hex'),
  } } } } };
};
const app = acp.agent({ name: 'hydra-direct-fixture', version: '1' })
  .onRequest(acp.methods.agent.initialize, async ctx => {
    if (['delayed-initialize', 'closing-initialize'].includes(scenario)) {
      log({ method: 'initialize-pending' });
      while (!existsSync(`${journal}.initialize`)) await new Promise(resolve => setTimeout(resolve, 20));
    }
    if (scenario === 'cold-recovery' && !existsSync(`${journal}.online`)) {
      log({ method: 'initialize-offline' });
      process.exit(18);
    }
    // Like scode, every advertising fixture echoes peer input a client declares.
    peerOwned = scenario === 'legacy' ? 0
      : Math.min(2, ctx.params.clientCapabilities?._meta?.sudocode?.peerInput?.version ?? 0);
    log({ method: 'initialize', peerOwned });
    return { protocolVersion: 1, agentCapabilities: { loadSession: true },
      ...(scenario === 'legacy' ? {} : { _meta: { sudocode: { directA2a: {
        version: 1, hostAudit: 1, ...(scenario === 'unadvertised-refusal' ? {} : { errorOutcomes: 1 }),
      }, ...(peerOwned ? { peerInput: { version: peerOwned } } : {}) } } }),
    };
  })
  .onRequest(acp.methods.agent.session.new, () => {
    sessionId=`session-${randomUUID()}`;log({method:'session/new',sessionId});return {sessionId};
  })
  .onRequest('_fixture/peer', value=>value, async ctx=>{
    const target=ctx.params.wrongSession?'another-session':sessionId, from=ctx.params.from;
    if(peerOwned)await ctx.client[peerOwned >= 2 ? 'request' : 'notify']('_sudocode/session/peer_message',{sessionId:target, deliveryId:'a'.repeat(64),
      text:`<mailbox-message from="${from}">\n${ctx.params.text}\n</mailbox-message>`,body:ctx.params.text,
      sudocode:{a2a:{from,kind:'message'}}});
    else await ctx.client.notify(acp.methods.client.session.update,{sessionId:target,
      update:{sessionUpdate:'user_message_chunk',content:{type:'text',text:ctx.params.text}},
      _meta:{sudocode:{a2a:{from,kind:'message'}}}});
    return {};
  })
  .onRequest(acp.methods.agent.session.load, ctx => {
    sessionId = ctx.params.sessionId; log({ method: 'session/load', sessionId });
    // A client that owns peer input hears peers even before its own startup completes.
    if (peerOwned && scenario.startsWith('channel')) setTimeout(() => { for (const from of ['fixture-peer', 'stranger']) {
      log({ method: 'peer-notification', from });
      void ctx.client[peerOwned >= 2 ? 'request' : 'notify']('_sudocode/session/peer_message', { sessionId, deliveryId:'b'.repeat(64),
        text: `<mailbox-message from="${from}" summary="greeting">\nhello from ${from}\n</mailbox-message>`,
        body: `hello from ${from}`, sudocode: { a2a: { from, kind: 'message', summary: 'greeting',
          verified: scenario === 'channel-node-namespace' } } }).catch(() => {});
    } }, 50);
    return {};
  })
  .onRequest(acp.methods.agent.session.prompt, async ctx => {
    log({ method: 'session/prompt' });
    if (scenario !== 'task') throw new Error('Unexpected model path');
    await ctx.client.notify(acp.methods.client.session.update, { sessionId,
      update: { sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: 'Task result' } } });
    return { stopReason: 'end_turn' };
  })
  .onRequest('_sudocode/a2a/send', value => value, ctx => {
    log({ method: '_sudocode/a2a/send', input: ctx.params });
    if (scenario === 'missing-method') throw acp.RequestError.methodNotFound('_sudocode/a2a/send');
    if (scenario === 'remote-invalid-params') throw acp.RequestError.invalidParams({ phase: 'post-execution-fixture' });
    if (['typed-refusal', 'typed-unknown', 'unadvertised-refusal', 'bad-refusal', 'old-refusal-code'].includes(scenario)) {
      const refused = scenario !== 'typed-unknown';
      throw new acp.RequestError(scenario === 'old-refusal-code' ? -32070 : refused ? -32602 : -32603, 'fixture control state', {
        ...(refused ? {} : { outcome: 'unknown', detail: 'fixture after admission' }),
        sudocode: { directA2a: { version: scenario === 'bad-refusal' ? 2 : 1,
          outcome: refused ? 'not_submitted' : 'unknown' } },
      });
    }
    if (ctx.params.sessionId !== sessionId) throw new Error('Unknown native session');
    if (scenario === 'loss') process.exit(17); // Request journal exists, response is lost.
    if (scenario === 'malformed') return { queued: true };
    return result({ success: true, routing: { sender: actor, to: ctx.params.to }, text: ctx.params.message }, 'send', ctx.params);
  })
  .onRequest('_sudocode/a2a/agent_list', value => value, async ctx => {
    log({ method: '_sudocode/a2a/agent_list', input: ctx.params });
    const gate=process.env.SCODE_DIRECT_FIXTURE_LIST_GATE;
    while(gate&&!existsSync(gate))await new Promise(resolve=>setTimeout(resolve,10));
    const failure = process.env.SCODE_DIRECT_FIXTURE_INITIAL_LIST_FAILURE;
    if (scenario.startsWith('initial-list-unavailable') && failure && !existsSync(failure)) {
      writeFileSync(failure, JSON.stringify({pid:process.pid,sessionId}), {flag:'wx'});
      returnedInitialListFailure = true;
      return result('list recipients at /agents: status: Unavailable, message: "tcp connect error", details: [], metadata: MetadataMap { headers: {} }', 'agent_list', ctx.params, true);
    }
    return result({ agents: [{ name: 'fixture-peer', send_to: 'fixture-peer' }] }, 'agent_list', ctx.params);
  })
  .onRequest(acp.methods.agent.session.close, () => {
    if (scenario.startsWith('initial-list-unavailable')) log({method:'session/close',sessionId});
    if (scenario === 'initial-list-unavailable-cleanup-unknown' && returnedInitialListFailure) {
      // POSIX-only test: this real bounded descendant holds the original stderr
      // pipe past the close deadline even after its direct parent exits.
      const drained = process.env.SCODE_DIRECT_FIXTURE_INITIAL_LIST_FAILURE+'.drained';
      const holder = spawn(process.execPath, ['-e',
        'setTimeout(()=>require("node:fs").writeFileSync(process.argv[1],"drained"),4000)', drained],
        {stdio:['ignore','ignore',process.stderr]});
      holder.unref();
    }
    return {};
  });
const connection = app.connect(acp.ndJsonStream(Writable.toWeb(process.stdout), Readable.toWeb(process.stdin)));
await connection.closed;
