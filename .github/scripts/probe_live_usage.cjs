// Record usage and scrubbed provider errors; exclude generated content and credentials.
const { clean, writeEvidence } = require('./live_model_evidence.cjs');

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
  const base = process.env.PROXY_BASE_URL.replace(/\/$/, '');
  const models = [...new Set([process.env.SCODE_LIVE_MODEL, process.env.SCODE_NATIVE_MODEL].filter(Boolean))];
  const errorDetails = body => {
    const error = body?.error ?? body;
    return { type: clean(error?.type), code: typeof error?.code === 'number' ? error.code : clean(error?.code), message: clean(error?.message ?? error?.msg) };
  };
  const report = [];
  for (const model of models) for (const route of ['chat/completions', 'messages']) {
    const item = { route, model, usage: [] };
    report.push(item);
    try {
      const body = { model, messages: [{ role: 'user', content: 'Reply exactly: pong' }],
        max_tokens: 32, stream: true };
      if (route === 'chat/completions') body.stream_options = { include_usage: true };
      const response = await fetch(`${base}/${route}`, {
        method: 'POST', signal: AbortSignal.timeout(60000),
        headers: { authorization: `Bearer ${token}`, 'x-api-key': token,
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
          const usage = frame.usage ?? frame.message?.usage ?? frame.response?.usage;
          if (usage) {
            item.usage.push({ event: frame.type, keys: Object.keys(usage),
              source: usage.billing_usage?.source, semantic: usage.billing_usage?.semantic,
              counts: numericFields(usage) });
          }
          if (['message_stop', 'response.completed', 'response.failed'].includes(frame.type)) {
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
  console.log(writeEvidence(process.argv[2], report));
}
main().catch(error => { console.error(error.message); process.exitCode = 1; });
