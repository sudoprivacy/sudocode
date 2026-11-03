const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
const { execFileSync } = require('node:child_process');
const git = require('./sourceGit.cjs');
const root = path.resolve(__dirname, '..');
const output = path.resolve(process.argv[2] || path.join(root, 'artifacts'));
const manifest = JSON.parse(fs.readFileSync(path.join(root, 'manifest.json'), 'utf8'));
if (manifest.dirty || git('status', '--porcelain=v1') || manifest.sourceCommit !== git('rev-parse', 'HEAD')) {
  throw new Error('Build and package a clean committed source tree');
}
for (const [name, expected] of Object.entries(manifest.files)) {
  const bytes = fs.readFileSync(path.join(root, name));
  if (bytes.length !== expected.bytes || crypto.createHash('sha256').update(bytes).digest('hex') !== expected.sha256) {
    throw new Error(`Build input changed: ${name}`);
  }
}
if (!process.env.npm_execpath) throw new Error('Use npm run pack:runtime');
fs.mkdirSync(output, { recursive: true });
const packed = JSON.parse(execFileSync(process.execPath, [process.env.npm_execpath, 'pack',
  '--ignore-scripts', '--json', '--pack-destination', output], { cwd: root, encoding: 'utf8' }));
const archive = path.join(output, packed[0].filename);
const bytes = fs.readFileSync(archive);
const receipt = { sourceCommit: manifest.sourceCommit, archive: path.basename(archive),
  bytes: bytes.length, sha256: crypto.createHash('sha256').update(bytes).digest('hex'),
  npmIntegrity: packed[0].integrity };
fs.writeFileSync(`${archive}.receipt.json`, JSON.stringify(receipt, null, 2) + '\n');
console.log(JSON.stringify({ ...receipt, path: archive }));
