const fs = require('node:fs');
const path = require('node:path');

const secrets = ['PROXY_AUTH_TOKEN', 'ANTHROPIC_API_KEY', 'OPENAI_API_KEY']
  .map(key => process.env[key]).filter(Boolean);

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

module.exports = { clean, writeEvidence };
