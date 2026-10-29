// Record usage and scrubbed provider errors; exclude generated content and credentials.
const { clean, writeEvidence, providerFailure } = require('./live_model_evidence.cjs');

function numericFields(value) {
  const result = {};
  for (const [key, field] of Object.entries(value ?? {})) {
    if (typeof field === 'number') result[key] = field;
    else if (field && typeof field === 'object' && !Array.isArray(field)) {
      result[key] = numericFields(field);
    }
  }
  return result;
}

async function main() {
  const token = process.env.PROXY_AUTH_TOKEN;
  if (!token) throw new Error('PROXY_AUTH_TOKEN is required');
  const nativeToken = process.env.NATIVE_PROXY_AUTH_TOKEN;
  if (process.env.SCODE_NATIVE_MODEL && !nativeToken) {
    throw new Error('NATIVE_PROXY_AUTH_TOKEN is required when SCODE_NATIVE_MODEL is set');
  }
  const azureToken = process.env.AZURE_PROXY_AUTH_TOKEN;
  if (process.env.SCODE_AZURE_MODEL && !azureToken) {
    throw new Error('AZURE_PROXY_AUTH_TOKEN is required when SCODE_AZURE_MODEL is set');
  }
  const base = process.env.PROXY_BASE_URL.replace(/\/$/, '');
  const targets = [
    { model: process.env.SCODE_LIVE_MODEL, token, profile: 'default' },
    { model: process.env.SCODE_NATIVE_MODEL, token: nativeToken, profile: 'native' },
    { model: process.env.SCODE_AZURE_MODEL, token: azureToken, profile: 'azure' },
  ].filter(target => target.model);
  if (process.argv.includes('--require-ready') && targets.length === 0) {
    throw new Error('At least one model is required for provider preflight');
  }
  const errorDetails = body => {
    const error = body?.error ?? body;
    return { type: clean(error?.type), code: typeof error?.code === 'number' ? error.code : clean(error?.code), message: clean(error?.message ?? error?.msg) };
  };
  const report = [];
  const catalog = { route: 'models', profile: 'default' };
  report.push(catalog);
  try {
    const response = await fetch(`${base}/models`, {
      signal: AbortSignal.timeout(15000),
      headers: { authorization: `Bearer ${token}` },
    });
    catalog.status = response.status;
    const body = await response.json();
    if (response.ok) {
      catalog.models = (body.data ?? []).map(model => clean(model.id, 256)).filter(Boolean).sort();
    } else {
      catalog.providerError = errorDetails(body);
    }
  } catch (error) {
    catalog.error = error.name;
  }
  for (const { model, token: modelToken, profile } of targets) for (const route of ['chat/completions', 'messages']) {
    const item = { route, model, profile, usage: [] };
    report.push(item);
    try {
      const body = { model, messages: [{ role: 'user', content: 'Reply exactly: pong' }],
        max_tokens: 32, stream: true };
      if (route === 'chat/completions') body.stream_options = { include_usage: true };
      const response = await fetch(`${base}/${route}`, {
        method: 'POST', signal: AbortSignal.timeout(60000),
        headers: { authorization: `Bearer ${modelToken}`, 'x-api-key': modelToken,
          'anthropic-version': '2023-06-01', 'content-type': 'application/json' },
        body: JSON.stringify(body),
      });
      item.status = response.status;
      item.gatewayRequestId = response.headers.get('x-oneapi-request-id');
      if (!response.ok) {
        try { item.providerError = errorDetails(await response.json()); }
        catch { item.errorBodyFormat = 'non-json'; }
        continue;
      }
      const decoder = new TextDecoder();
      let buffer = '', done = false;
      for await (const chunk of response.body) {
        buffer += decoder.decode(chunk, { stream: true });
        let boundary;
        while ((boundary = buffer.indexOf('\n')) >= 0) {
          const line = buffer.slice(0, boundary).trim();
          buffer = buffer.slice(boundary + 1);
          if (!line.startsWith('data:')) continue;
          const data = line.slice(5).trim();
          if (data === '[DONE]') { done = true; break; }
          let frame;
          try { frame = JSON.parse(data); } catch { continue; }
          if (['error', 'response.failed'].includes(frame.type)) {
            item.providerError = errorDetails(frame.error ?? frame.response?.error);
            done = true;
            break;
          }
          const usage = frame.usage ?? frame.message?.usage ?? frame.response?.usage;
          if (usage) {
            item.usage.push({ event: frame.type, keys: Object.keys(usage),
              source: usage.billing_usage?.source, semantic: usage.billing_usage?.semantic,
              counts: numericFields(usage) });
          }
          if (['message_stop', 'response.completed'].includes(frame.type)) {
            done = true; break;
          }
        }
        if (done) break;
      }
      item.terminalSeen = done;
    } catch (error) {
      item.error = error.name;
    }
  }
  for (const item of report) {
    item.failureCategory = providerFailure(item.providerError && item.status === 200 ? undefined : item.status,
      item.providerError ?? item.error);
  }
  console.log(writeEvidence(process.argv[2], report));
  if (process.argv.includes('--require-ready')) {
    for (const { model, profile } of targets) {
      const routes = report.filter(item => item.model === model && item.profile === profile);
      if (routes.some(item => item.status >= 200 && item.status < 300 && item.terminalSeen && !item.providerError)) continue;
      const category = routes.find(item => item.failureCategory)?.failureCategory ?? 'protocol';
      console.error(`::error title=Live provider preflight (${category})::${clean(profile)}/${clean(model)} has no completed provider route. See the retained provider evidence; model acceptance has not passed.`);
      process.exitCode = 1;
    }
  }
}
main().catch(error => { console.error(error.message); process.exitCode = 1; });
