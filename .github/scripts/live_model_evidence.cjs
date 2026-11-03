const fs = require('node:fs');
const path = require('node:path');

const secrets = ['PROXY_AUTH_TOKEN', 'NATIVE_PROXY_AUTH_TOKEN', 'AZURE_PROXY_AUTH_TOKEN', 'ANTHROPIC_API_KEY', 'OPENAI_API_KEY', 'GH_TOKEN', 'GITHUB_TOKEN']
  .map(key => process.env[key]).filter(Boolean).sort((left, right) => right.length - left.length);

function clean(value, limit = 2048) {
  if (typeof value !== 'string') return undefined;
  for (const secret of secrets) value = value.split(secret).join('[REDACTED]');
  return value.replace(/\bsk-[A-Za-z0-9_-]{8,}\b/g, '[REDACTED]').slice(0, limit);
}

function writeEvidence(output, report) {
  const serialized = JSON.stringify(report, null, 2) + '\n';
  if (secrets.some(secret => serialized.includes(secret))) {
    throw new Error('Refusing to write evidence containing a credential');
  }
  fs.mkdirSync(path.dirname(output), { recursive: true });
  fs.writeFileSync(output, serialized);
  return serialized;
}

function providerFailure(status, error) {
  status = Number.parseInt(status, 10);
  if (status >= 200 && status < 300) return undefined;
  const details = typeof error === 'string' ? error : `${error?.code ?? ''} ${error?.message ?? ''}`;
  if (/pre_consume_token_quota_failed|insufficient_quota|quota_exceeded|额度不足/i.test(details)) return 'quota';
  if (status === 401) return 'authentication';
  if (status === 403) return 'authorization';
  if (status === 429) return 'rate_limit';
  if (status >= 500 && status < 600) return 'service';
  if (/model_not_found|no_available_channel|可用渠道不存在/i.test(details)) return 'configuration';
  if (/^(AbortError|TimeoutError|TypeError)$/.test(details)) return 'transport';
  return undefined;
}

module.exports = { clean, writeEvidence, providerFailure };
