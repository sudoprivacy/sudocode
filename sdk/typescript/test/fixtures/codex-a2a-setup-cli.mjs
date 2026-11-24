// Stands in for the native `codex` command in one-command setup tests. Like
// the real one it keeps the MCP entry in CODEX_HOME/config.toml, and adding an
// entry replaces it together with its tool tables.
import fs from 'node:fs';
import net from 'node:net';
import path from 'node:path';

const home = process.env.CODEX_HOME;
if (!home) { console.error('fixture requires CODEX_HOME'); process.exit(9); }
const args = process.argv.slice(2);
const command = args.join(' ');
const config = path.join(home, 'config.toml');
const daemon = path.join(home, 'fixture-daemon-running');
if (!(args[0] === 'mcp' && args[1] === 'get') && !command.startsWith('app-server daemon version')) {
  fs.appendFileSync(path.join(home, 'codex-cli.log'), JSON.stringify(args) + '\n');
}
const read = () => { try { return fs.readFileSync(config, 'utf8'); } catch (error) { if (error.code === 'ENOENT') return ''; throw error; } };
/** The file as [header, body] tables; the text before the first header has header ''. */
const tables = text => {
  const found = [['', '']];
  for (const line of text.split('\n')) {
    const header = /^\[([^\]]+)\]\s*$/.exec(line);
    if (header) found.push([header[1], '']); else found.at(-1)[1] += line + '\n';
  }
  return found;
};
const write = found => fs.writeFileSync(config, found.map(([header, body]) => (header ? `[${header}]\n` : '') + body).join('').replace(/\n{3,}/g, '\n\n'));
const ours = header => header === 'mcp_servers.nexus_scode' || header.startsWith('mcp_servers.nexus_scode.');

if (command === 'mcp get nexus_scode --json') {
  const entry = tables(read()).find(([header]) => header === 'mcp_servers.nexus_scode');
  if (!entry) { console.error("Error: No MCP server named 'nexus_scode' found."); process.exit(1); }
  const value = name => JSON.parse(new RegExp(`^${name} = (.*)$`, 'm').exec(entry[1])[1]);
  console.log(JSON.stringify({ name: 'nexus_scode', enabled: true, disabled_reason: null,
    transport: { type: 'stdio', command: value('command'), args: value('args'), env: null, env_vars: [], cwd: null } }, null, 2));
} else if (args.slice(0, 4).join(' ') === 'mcp add nexus_scode --' && args.length > 4) {
  const kept = tables(read()).filter(([header]) => !ours(header));
  kept.push(['mcp_servers.nexus_scode', `command = ${JSON.stringify(args[4])}\nargs = ${JSON.stringify(args.slice(5))}\n\n`]);
  write(kept);
  console.log("Added global MCP server 'nexus_scode'.");
} else if (command === 'mcp remove nexus_scode') {
  write(tables(read()).filter(([header]) => !ours(header)));
} else if (command === 'app-server daemon version') {
  console.log(JSON.stringify({ status: fs.existsSync(daemon) ? 'running' : 'stopped' }));
} else if (command === 'app-server daemon start') {
  if (!process.env.CODEX_FIXTURE_DAEMON_BROKEN) fs.writeFileSync(daemon, '');
  console.log(JSON.stringify({ status: 'started' }));
} else if (args[0] === 'app-server' && args[1] === 'proxy') {
  const socket = net.createConnection({ host: '127.0.0.1', port: Number(process.env.SCODE_SETUP_PROXY_PORT) });
  process.stdin.pipe(socket); socket.pipe(process.stdout);
  socket.on('error', error => { console.error(error); process.exitCode = 1; });
  socket.on('close', () => process.exit());
} else {
  console.error('unexpected codex invocation: ' + JSON.stringify(args));
  process.exit(3);
}
