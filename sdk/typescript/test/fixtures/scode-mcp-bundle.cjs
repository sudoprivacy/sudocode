const Module = require('node:module');
const originalLoad = Module._load;
// Copied CLI runtimes have @hydra/core but need not have CLI-only npm deps.
// Fail if the production bundle accidentally relies on workspace hoisting.
Module._load = function (name, ...args) {
  if (/^(@modelcontextprotocol\/|zod(?:\/|$)|ws$)/.test(name)) {
    throw new Error(`MCP dependency escaped the runtime bundle: ${name}`);
  }
  return originalLoad.call(this, name, ...args);
};
require(process.argv[2]).runScodeMcp(process.argv[3])
  .catch(error => { console.error(error); process.exitCode = 1; });
