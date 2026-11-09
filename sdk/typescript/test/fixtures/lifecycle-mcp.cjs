// Same shared runtime/signal adapter as server.cjs, plus a test-only IPC route
// into Node's signal event. No production hook or injected close implementation.
const {installCommandShutdownSignals,runScodeMcp}=require('../../dist/index.cjs');
installCommandShutdownSignals();
process.on('message',value=>{if(value==='SIGTERM'||value==='SIGINT')process.emit(value);});
void runScodeMcp(process.argv[2]).catch(error=>{console.error(error);process.exitCode=1;});
