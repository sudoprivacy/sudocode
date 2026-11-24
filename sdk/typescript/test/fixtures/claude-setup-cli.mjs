// Stands in for Claude Code's `claude` command in setup tests. It implements
// only the two user-scope MCP sub-commands setup may call; anything else,
// including `mcp get` (which would start the server), fails the test.
import fs from 'node:fs';
import path from 'node:path';

const directory = process.env.CLAUDE_CONFIG_DIR;
if (!directory) { console.error('fixture requires CLAUDE_CONFIG_DIR'); process.exit(9); }
const file = path.join(directory, '.claude.json');
const args = process.argv.slice(2);
fs.mkdirSync(directory, { recursive: true });
fs.appendFileSync(path.join(directory, 'claude-cli.log'), JSON.stringify(args) + '\n');
const read = () => {
  try { return JSON.parse(fs.readFileSync(file, 'utf8')); } catch (error) { if (error.code === 'ENOENT') return {}; throw error; }
};
if (process.env.CLAUDE_FIXTURE_FAIL) { console.error('claude fixture was told to fail'); process.exit(4); }
if (args[0] === 'mcp' && args[1] === 'add-json' && args.length === 6 && args[4] === '-s' && args[5] === 'user') {
  const config = read();
  config.mcpServers ??= {};
  // Like the real command: an existing name is reported, kept, and still exits 0.
  if (config.mcpServers[args[2]]) { console.log(`MCP server ${args[2]} already exists in user config`); process.exit(0); }
  if (!process.env.CLAUDE_FIXTURE_DROP_ADD) config.mcpServers[args[2]] = JSON.parse(args[3]);
  fs.writeFileSync(file, JSON.stringify(config, null, 2));
  console.log(`Added stdio MCP server ${args[2]} to user config`);
} else if (args[0] === 'mcp' && args[1] === 'remove' && args.length === 5 && args[3] === '-s' && args[4] === 'user') {
  const config = read();
  if (!config.mcpServers?.[args[2]]) { console.error(`No MCP server found with name: ${args[2]}`); process.exit(1); }
  delete config.mcpServers[args[2]];
  fs.writeFileSync(file, JSON.stringify(config, null, 2));
} else {
  console.error('unexpected claude invocation: ' + JSON.stringify(args));
  process.exit(3);
}
