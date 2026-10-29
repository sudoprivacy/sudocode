// Retain enough information to correlate a live failure with its gateway log.
// The artifact uses an explicit field list; request bodies and arbitrary
// headers stay out of it. Run after both successful and failed live tests.
const fs = require('node:fs');

const [input, output] = process.argv.slice(2);
if (!input || !output) throw new Error('Usage: summarize_live_model_trace.cjs INPUT OUTPUT');
const { clean, writeEvidence, providerFailure } = require('./live_model_evidence.cjs');
const report = { traceFiles: 0, malformedLines: 0, requests: [] };
const responses = new Map();
const refusals = new Map();
const usages = new Map();
const streamErrors = new Map();
const failures = new Map();
const progress = new Map();
for (const file of [3, 2, 1, 0].map(n => n ? `${input}.${n}` : input)) {
  if (!fs.existsSync(file)) continue;
  report.traceFiles++;
  for (const line of fs.readFileSync(file, 'utf8').split(/\r?\n/)) {
    if (!line.trim()) continue;
    let event;
    try { event = JSON.parse(line); } catch { report.malformedLines++; continue; }
    const data = event.attributes ?? {};
    if (event.event === 'response_debug') {
      const ids = {};
      for (const [key, value] of Object.entries(data.headers ?? {})) {
        if (['request-id', 'x-request-id', 'x-client-request-id', 'x-oneapi-request-id'].includes(key.toLowerCase())) {
          ids[key.toLowerCase()] = clean(value, 256);
        }
      }
      responses.set(data.request_id, { status: data.status, ids });
    } else if (event.event === 'response_usage') {
      usages.set(data.request_id, {
        inputTokens: data.input_tokens, outputTokens: data.output_tokens,
        cacheReadTokens: data.cache_read_input_tokens,
        cacheWriteTokens: data.cache_creation_input_tokens,
        reportedTotalTokens: data.reported_total_tokens,
      });
    } else if (event.event === 'request_failed') {
      const rows = failures.get(data.request_id) ?? [];
      rows.push({ attempt: data.attempt, retryable: data.retryable,
        durationMs: data.duration_ms, error: clean(data.error) });
      failures.set(data.request_id, rows);
    } else if (event.event === 'response_stream_progress') {
      const rows = progress.get(data.request_id) ?? [];
      rows.push({ phase: clean(data.phase, 64), chunks: data.chunks_read, bytes: data.bytes_read,
        bufferedBytes: data.buffered_bytes, unframedJson: data.unframed_json,
        frames: data.sse_events_read, events: data.normalized_events,
        textDeltas: data.text_deltas, thinkingDeltas: data.thinking_deltas,
        toolDeltas: data.tool_deltas, elapsedMs: data.elapsed_ms, finished: data.finished });
      progress.set(data.request_id, rows);
    } else if (event.event === 'stream_error') {
      streamErrors.set(data.request_id, {
        chunksRead: data.chunks_read, eventsRead: data.sse_events_read,
        usageSeen: data.usage_seen, streamFinished: data.stream_finished,
        message: clean(data.error_message),
      });
    } else if (event.event === 'provider_refusal') {
      refusals.set(data.request_id, {
        model: clean(data.model, 256), category: clean(data.category, 256),
        explanation: clean(data.explanation),
      });
    } else if (event.event === 'request_debug') {
      const body = data.body ?? {};
      const messages = Array.isArray(body.messages) ? body.messages : [];
      const system = typeof body.system === 'string' ? body.system
        : (Array.isArray(body.system) ? body.system.map(block => block.text ?? '').join('\n')
          : messages.filter(message => message.role === 'system')
            .map(message => typeof message.content === 'string' ? message.content : '').join('\n'));
      const environment = system.split(/\r?\n/)
        .filter(line => /^\s*-\s*(Working directory|Platform):/.test(line))
        .slice(0, 20).map(line => clean(line));
      const assistantTools = [];
      const captureCall = (name, args) => {
        if (typeof args === 'string') {
          try { args = JSON.parse(args); } catch { args = {}; }
        }
        assistantTools.push({ name: clean(name, 256), path: clean(args?.path ?? args?.file_path) });
      };
      for (const message of messages) {
        if (message.role !== 'assistant') continue;
        for (const block of Array.isArray(message.content) ? message.content : []) {
          if (block.type !== 'tool_use') continue;
          captureCall(block.name, block.input);
        }
        for (const call of Array.isArray(message.tool_calls) ? message.tool_calls : []) {
          captureCall(call.function?.name, call.function?.arguments);
        }
      }
      for (const item of Array.isArray(body.input) ? body.input : []) {
        if (item.type === 'function_call') captureCall(item.name, item.arguments);
      }
      let origin, route;
      try { const url = new URL(data.url); origin = url.origin; route = url.pathname; } catch {}
      report.requests.push({ requestId: clean(data.request_id, 256),
        sessionId: clean(event.session_id, 256), origin: clean(origin), route: clean(route, 256),
        model: clean(body.model, 256), messageCount: body.messages?.length ?? 0,
        environment, assistantTools });
    }
  }
}
for (const request of report.requests) {
  request.response = responses.get(request.requestId);
  const ids = [request.requestId, ...Object.values(request.response?.ids ?? {})];
  const find = map => ids.map(id => map.get(id)).find(value => value !== undefined);
  request.refusal = find(refusals);
  request.usage = find(usages);
  request.streamError = find(streamErrors);
  request.failures = find(failures);
  request.streamProgress = find(progress);
  request.providerFailure = providerFailure(request.streamError ? undefined : request.response?.status,
    request.streamError?.message ?? request.failures?.map(failure => failure.error).join('\n'));
  if (request.providerFailure) {
    console.error(`::error title=Live provider failure (${request.providerFailure})::Request ${request.requestId} could not complete provider acceptance. See the retained evidence.`);
  }
}
writeEvidence(output, report);
console.log(`Live model evidence: ${report.requests.length} requests, ${report.malformedLines} malformed lines`);
