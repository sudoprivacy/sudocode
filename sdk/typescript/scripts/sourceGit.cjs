const path = require('node:path');
const { execFileSync } = require('node:child_process');

// Git hooks export repository-local variables. A cwd alone cannot override
// those, particularly in linked worktrees or when building from a subdirectory.
// Query this source checkout, never the invoking hook's index/object directory.
const env = { ...process.env };
for (const name of execFileSync('git', ['rev-parse', '--local-env-vars'], {
  encoding: 'utf8',
}).trim().split('\n')) delete env[name];

module.exports = (...args) => execFileSync('git', args, {
  cwd: path.resolve(__dirname, '..'), env, encoding: 'utf8',
}).trim();
