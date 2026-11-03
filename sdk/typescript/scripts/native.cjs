const path = require('node:path');
const { spawnSync } = require('node:child_process');
const python = process.env.SCODE_TEST_PYTHON || (process.platform === 'win32' ? 'python' : 'python3');
const result = spawnSync(python, [path.join(__dirname, '../test/nativeFixture.py')], {
  stdio: 'inherit', env: process.env,
});
if (result.error) throw result.error;
process.exitCode = result.status ?? 1;
