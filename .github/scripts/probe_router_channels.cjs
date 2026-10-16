// Temporary, read-only routing diagnostics. Credentials remain in the runner.
const { clean, writeEvidence } = require('./live_model_evidence.cjs');

const origin = 'https://api.sudorouter.ai';
const model = 'claude-sonnet-4-6';
const token = process.env.PROXY_AUTH_TOKEN;
const report = { origin, model, metadata: {}, probes: [] };
if (!token) throw new Error('PROXY_AUTH_TOKEN is required');

async function get(path) {
  const response = await fetch(`${origin}${path}`, {
    signal: AbortSignal.timeout(30000),
    headers: { authorization: `Bearer ${token}`, 'user-agent': 'scode-ci-routing-diagnostics' },
  });
  const body = await response.json();
  return { status: response.status, body };
}

async function logs() {
  const { status, body } = await get('/api/log/token');
  const rows = Array.isArray(body.data) ? body.data : [];
  const selected = rows.filter(row => /claude.*sonnet/i.test(row.model_name ?? '')).map(row => ({
    model: clean(row.model_name), channel: row.channel, group: clean(row.group),
    type: row.type, createdAt: row.created_at, requestId: clean(row.request_id),
  }));
  return { status, success: body.success, rows: selected };
}

async function probe(channel, probeModel = model) {
  const item = { model: probeModel, channel: channel ?? 'automatic', maxTokens: 64000, thinkingBudget: 32000 };
  report.probes.push(item);
  try {
    const routedToken = channel === undefined ? token : `${token}-${channel}`;
    const response = await fetch(`${origin}/v1/messages`, {
      method: 'POST', signal: AbortSignal.timeout(90000),
      headers: {
        authorization: `Bearer ${routedToken}`, 'x-api-key': routedToken,
        'anthropic-version': '2023-06-01', 'content-type': 'application/json',
      },
      body: JSON.stringify({ model: probeModel, max_tokens: 64000, thinking: { type: 'enabled', budget_tokens: 32000 },
        messages: [{ role: 'user', content: 'Reply exactly: pong' }], stream: true }),
    });
    item.status = response.status;
    item.requestId = clean(response.headers.get('x-oneapi-request-id'));
    if (!response.ok) {
      const body = await response.json();
      const error = body.error ?? body;
      item.error = { type: clean(error.type), code: clean(String(error.code ?? '')),
        message: clean(error.message ?? error.msg) };
      return;
    }
    const decoder = new TextDecoder();
    let buffer = '';
    item.events = [];
    for await (const chunk of response.body) {
      buffer += decoder.decode(chunk, { stream: true });
      let boundary;
      while ((boundary = buffer.indexOf('\n')) >= 0) {
        const line = buffer.slice(0, boundary).trim();
        buffer = buffer.slice(boundary + 1);
        if (!line.startsWith('data:')) continue;
        let frame;
        try { frame = JSON.parse(line.slice(5).trim()); } catch { continue; }
        if (frame.type && !item.events.includes(frame.type)) item.events.push(clean(frame.type));
        if (frame.message?.model) item.responseModel = clean(frame.message.model);
        if (frame.type === 'error') item.streamError = clean(frame.error?.message);
        if (frame.type === 'message_stop') { item.terminalSeen = true; return; }
      }
    }
    item.terminalSeen = false;
  } catch (error) { item.exception = clean(error.name); }
}

async function main() {
  const models = await get('/v1/models');
  report.metadata.models = { status: models.status,
    sonnet: (models.body.data ?? []).filter(row => /sonnet/i.test(row.id)).map(row => ({
      id: clean(row.id), supportedEndpoints: row.supported_endpoint_types,
    })) };
  const usage = await get('/api/usage/token/');
  const usageData = usage.body.data ?? {};
  report.metadata.token = { status: usage.status, unlimitedQuota: usageData.unlimited_quota,
    modelLimitsEnabled: usageData.model_limits_enabled, modelLimits: usageData.model_limits };
  report.logsBefore = await logs();
  await probe();
  for (const alternative of ['claude-sonnet-4-5-20250929', 'claude-sonnet-5-5']) {
    if (report.metadata.models.sonnet.some(row => row.id === alternative)) await probe(undefined, alternative);
  }
  report.logsAfterAutomatic = await logs();
  const candidates = [...new Set([...report.logsAfterAutomatic.rows, ...report.logsBefore.rows]
    .map(row => row.channel).filter(id => Number.isSafeInteger(id) && id > 0))].slice(0, 6);
  for (const channel of candidates) {
    await probe(channel);
    const last = report.probes.at(-1);
    if (last.status === 403 && /普通用户|Ordinary users/i.test(last.error?.message ?? '')) break;
  }
  report.logsAfter = await logs();
  console.log(writeEvidence(process.argv[2], report));
}
main().catch(error => {
  report.exception = clean(error.name);
  console.error(clean(error.message));
  console.log(writeEvidence(process.argv[2], report));
  process.exitCode = 1;
});
