const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { execFileSync } = require('node:child_process');
const root = path.resolve(__dirname, '..');
const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'scode-acp-package-'));
const run = (file, args, options = {}) => execFileSync(file, args, {
  cwd: directory, encoding: 'utf8', env: { ...process.env, NODE_PATH: '' }, ...options,
});
try {
  const archiveDir = path.join(directory, 'archives');
  const packed = JSON.parse(run(process.execPath, [path.join(root, 'scripts/pack.cjs'), archiveDir]));
  const install = path.join(directory, 'install');
  run(process.execPath, [process.env.npm_execpath, 'install', '--offline', '--ignore-scripts',
    '--no-audit', '--no-fund', '--prefix', install, packed.path]);
  const modules = path.join(install, 'node_modules');
  assert.deepEqual(fs.readdirSync(modules).filter(name => !name.startsWith('.')), ['@sudocode'],
    'offline package has no runtime dependency installation');
  // Compile a real installed-package consumer with legacy Node resolution too.
  const consumer = path.join(install, 'consumer.ts');
  fs.writeFileSync(consumer, `import { connectScode } from '@sudocode/acp/scodeAcp';
import { sendScodeMessage } from '@sudocode/acp/scodeA2a';
import { runScodeMcp } from '@sudocode/acp/mcp';
import { connectCodex } from '@sudocode/acp';
import { forwardScodeToCodex } from '@sudocode/acp/codexCallback';
void [connectScode, sendScodeMessage, runScodeMcp, connectCodex, forwardScodeToCodex];
`);
  run(process.execPath, [path.join(root, 'node_modules/typescript/bin/tsc'),
    '--noEmit', '--strict', '--skipLibCheck', '--module', 'commonjs', '--moduleResolution', 'node',
    '--target', 'ES2022', '--lib', 'ES2022', '--typeRoots', path.join(root, 'node_modules/@types'), consumer]);
  const entry = path.join(modules, '@sudocode/acp/dist/server.cjs');
  const verified = JSON.parse(run(process.execPath, [entry, '--verify']));
  assert.equal(verified.verified, true);
  assert.equal(verified.sourceCommit, packed.sourceCommit);
  const version = JSON.parse(run(process.execPath, [entry, '--version']));
  assert.equal(version.sourceCommit, packed.sourceCommit);
  const config = path.join(directory, 'private/runtime.json');
  const init = [entry, '--init', '--config', config, '--scode', process.execPath,
    '--workspace', directory, '--session-id', 'explicit-existing-session'];
  assert.equal(JSON.parse(run(process.execPath, init)).startedAgent, false);
  const bytes = fs.readFileSync(config);
  assert.equal(JSON.parse(bytes).acpSessionId, 'explicit-existing-session');
  assert.throws(() => run(process.execPath, init, { stdio: 'pipe' }), /EEXIST/);
  assert.deepEqual(fs.readFileSync(config), bytes);
  if (process.platform !== 'win32') {
    assert.equal(fs.statSync(config).mode & 0o777, 0o600);
    assert.equal(fs.statSync(path.dirname(config)).mode & 0o777, 0o700);
  }
  const autoConfig=path.join(directory,'private/codex-network.json');
  const initCodex=[entry,'--init-codex','--config',autoConfig,'--native-codex',process.execPath,
    '--enrollment',JSON.stringify([process.execPath,'operator-owned-adapter.cjs']),'--peers','["fixture-peer"]'];
  const prepared=JSON.parse(run(process.execPath,initCodex));
  assert.equal(prepared.installed,false);
  assert.deepEqual(prepared.registrationArgv,[process.execPath,'mcp','add','nexus_scode','--',process.execPath,fs.realpathSync(entry),'--codex','--config',autoConfig]);
  const autoBytes=fs.readFileSync(autoConfig);
  const auto=JSON.parse(autoBytes);
  assert.equal(auto.mode,'codex-autojoin');assert.match(auto.endpoint.token,/^[0-9a-f]{64}$/);
  assert.equal(auto.acpSessionId,undefined,'global config must not bind every root to one session');
  assert.equal(auto.workspace,undefined,'global config must not infer identity from a workspace');
  assert.throws(()=>run(process.execPath,initCodex,{stdio:'pipe'}),/EEXIST/);
  assert.deepEqual(fs.readFileSync(autoConfig),autoBytes);
  const output = run(process.execPath, [path.join(root, 'dist/testing/mcpIntegration.cjs'), entry]);
  process.stdout.write(output);
  process.stdout.write(run(process.execPath, [path.join(root, 'dist/testing/codexConnectionIntegration.cjs'), path.join(modules, '@sudocode/acp/dist/index.cjs')]));
  fs.appendFileSync(path.join(modules, '@sudocode/acp/dist/index.cjs'), '\n');
  assert.throws(() => run(process.execPath, [entry, '--verify'], { stdio: 'pipe' }), /integrity mismatch/);
  console.log(`PASS portable package: offline install, source ${packed.sourceCommit}, exclusive config, stdio calls and corruption refusal`);
} finally {
  assert.equal(path.dirname(directory), path.resolve(os.tmpdir()));
  assert.ok(path.basename(directory).startsWith('scode-acp-package-'));
  fs.rmSync(directory, { recursive: true, force: true });
}
