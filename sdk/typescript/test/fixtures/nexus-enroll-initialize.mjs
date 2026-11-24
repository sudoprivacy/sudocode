// Stands in for `nexus-agent-enroll --initialize`: writes the local grant and
// issues nothing. Any other use fails the test.
import fs from 'node:fs';
import path from 'node:path';

const args = process.argv.slice(2);
const value = name => { const index = args.indexOf(name); return index < 0 ? undefined : args[index + 1]; };
if (!args.includes('--initialize') || args.length !== 9) { console.error('unexpected enrollment invocation: ' + JSON.stringify(args)); process.exit(3); }
if (process.env.NEXUS_FIXTURE_FAIL) { console.error('Error: node certificate names another Node'); process.exit(1); }
const grant = value('--grant');
const record = { version: 1, purpose: 'codex-a2a', endpoint: value('--endpoint'), nodeDataDirectory: value('--data-dir'),
  stateDirectory: path.join(path.dirname(grant), 'node-enrollment-operations'),
  ownerNodeIdentity: 'nexus://zone/root/node/1', caCertificateSha256: '00'.repeat(32), workspaceRoots: [value('--workspace-root')] };
try { fs.writeFileSync(grant, JSON.stringify(record, null, 2) + '\n', { flag: 'wx', mode: 0o600 }); }
catch (error) { console.error('Error: ' + error.message); process.exit(1); }
console.log(JSON.stringify({ endpoint: record.endpoint, grantFile: grant, issued: false, prepared: true }));
