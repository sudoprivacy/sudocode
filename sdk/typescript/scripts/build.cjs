const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
const git = require('./sourceGit.cjs');
const { build } = require('esbuild');
async function main() {
const root = path.resolve(__dirname, '..');
const shared = { absWorkingDir: root, bundle: true, platform: 'node', format: 'cjs', target: 'node20' };
const library = await build({ ...shared, entryPoints: ['src/index.ts'], outfile: 'dist/index.cjs', metafile: true });
if (Object.keys(library.metafile.inputs).some(name => /(?:node-pty|electron|grpc|packages\/desktop\/)/.test(name))) {
  throw new Error('The ACP client must not include an application, terminal or Nexus implementation');
}
await build({ ...shared, entryPoints: ['src/server.ts'], outfile: 'dist/server.cjs', plugins: [{
  name: 'reuse-library', setup(build) {
    build.onResolve({ filter: /^\.\/(mcp|codexMcp|lifecycle)$/ }, args => args.importer === path.join(root, 'src/server.ts')
      ? { path: './index.cjs', external: true } : undefined);
  },
}] });
for (const test of ['acpIntegration', 'mcpIntegration', 'codexConnectionIntegration', 'codexAutojoinIntegration', 'nativeQueueIntegration']) {
  await build({ ...shared, entryPoints: [`test/${test}.ts`], outfile: `dist/testing/${test}.cjs`,
    external: ['../dist/index.cjs'] });
}
fs.copyFileSync(path.join(root, '../../LICENSE'), path.join(root, 'LICENSE'));
const files = {};
for (const name of ['package.json', 'README.md', 'LICENSE', ...fs.readdirSync(path.join(root, 'dist'))
  .filter(name => /\.(cjs|d\.ts)$/.test(name)).map(name => `dist/${name}`)].sort()) {
  const bytes = fs.readFileSync(path.join(root, name));
  files[name] = { bytes: bytes.length, sha256: crypto.createHash('sha256').update(bytes).digest('hex') };
}
fs.writeFileSync(path.join(root, 'manifest.json'), JSON.stringify({ format: 1,
  sourceCommit: git('rev-parse', 'HEAD'), dirty: !!git('status', '--porcelain=v1'),
  node: process.version, dependenciesBundled: true, nativeAddons: false, files,
}, null, 2) + '\n');

}
main().catch(error => { console.error(error); process.exitCode = 1; });
