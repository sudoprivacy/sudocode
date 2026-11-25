const { spawn } = require('node:child_process');

const setupAndOwner = [
  ['dist/testing/acpIntegration.cjs'],
  ['dist/testing/codexConnectionIntegration.cjs'],
  ['test/codexMcpReconnect.cjs'],
  ['dist/testing/codexLaunchIntegration.cjs'],
  ['dist/testing/codexSetupIntegration.cjs'],
  ['dist/testing/claudeSetupIntegration.cjs'],
  ['dist/testing/codexA2aSetupIntegration.cjs'],
  ['dist/testing/mailboxAdmission.cjs'],
  ['dist/testing/codexMailboxMcpIntegration.cjs'],
  ['dist/testing/mailboxIntegration.cjs'],
  ['dist/testing/mailboxOwnerIntegration.cjs'],
  ['test/mailboxCloseDrain.cjs'],
  ['dist/testing/operatorEnrollmentIntegration.cjs'],
  ['dist/testing/nativeQueueIntegration.cjs'],
  ['test/nativeTuiGuard.cjs'],
  ['test/nativeSocket.cjs'],
  ['test/receiverOwner.cjs'],
  ['test/lifecycleSignals.cjs'],
];

// These suites each create private temporary roots and bind ephemeral ports.
// Their real renewal windows can run while the other fixtures make progress.
const autojoin = [
  ['dist/testing/codexAutojoinIntegration.cjs'],
];

function run(args) {
  const name = args.at(-1);
  const started = performance.now();
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, args, { stdio: 'inherit' });
    child.once('error', reject);
    child.once('close', (code, signal) => {
      console.log(JSON.stringify({ sdkTest: name, code, signal, seconds: (performance.now() - started) / 1000 }));
      if (code === 0) resolve();
      else reject(Object.assign(new Error(name + ' exited with ' + (signal || code)), { exitCode: code }));
    });
  });
}

async function main() {
  if (!process.env.npm_execpath) throw new Error('Use npm test to run the SDK fixtures');
  let failure;
  async function runSeries(cases) {
    for (const args of cases) {
      if (failure) return;
      try {
        await run(args);
      } catch (error) {
        failure ??= error;
      }
    }
  }
  await Promise.all([runSeries(setupAndOwner), runSeries(autojoin)]);
  if (failure) throw failure;
  // The package fixture rebuilds dist and the manifest. Keep it after both
  // series have finished reading those files.
  await run(['test/portable.cjs']);
}

main().catch(error => {
  console.error(error);
  process.exitCode = error.exitCode ?? 1;
});
