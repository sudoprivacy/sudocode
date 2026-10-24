from pathlib import Path
import subprocess,os,json,hashlib,socket,time,uuid,shutil
import tempfile
sdk=Path(__file__).resolve().parent.parent
root=Path(os.environ['SCODE_NATIVE_TEST_ROOT'] if 'SCODE_NATIVE_TEST_ROOT' in os.environ else tempfile.mkdtemp(prefix='scode-acp-native-')).resolve()
root.mkdir(parents=True,exist_ok=True)
run=root/('run-'+uuid.uuid4().hex[:10]);run.mkdir()
node=Path(os.environ.get('npm_node_execpath') or shutil.which('node'))
npm=Path(os.environ['npm_execpath'])
scode=Path(os.environ['SCODE_TEST_BIN']).resolve()
daemon=Path(os.environ['NEXUSD_BIN']).resolve()
scode_hash=hashlib.sha256(scode.read_bytes()).hexdigest()
daemon_hash=hashlib.sha256(daemon.read_bytes()).hexdigest()
assert scode_hash==os.environ['SCODE_TEST_SHA256'].lower()
assert daemon_hash==os.environ['NEXUSD_SHA256'].lower()
scodeReceipt={'binarySha256':scode_hash,'sourceCommit':os.environ['SCODE_TEST_SOURCE']}
daemonReceipt={'binarySha256':daemon_hash,'tag':os.environ['NEXUSD_TEST_TAG']}
sock=socket.socket();sock.bind(('127.0.0.1',0));port=sock.getsockname()[1];sock.close()
data=run/'data';identity=run/'id';data.mkdir();identity.mkdir()
# Keep model/operator credentials and real node discovery out of every test child.
env={k:v for k,v in os.environ.items() if k.upper() in ['PATH','SYSTEMROOT','WINDIR','COMSPEC','PATHEXT','TEMP','TMP','SYSTEMDRIVE','NUMBER_OF_PROCESSORS','PROCESSOR_ARCHITECTURE']}
env.update(NEXUS_DATA_DIR=str(data),NEXUS_IDENTITY_DIR=str(identity),NEXUS_API_KEY_SECRET='isolated-acp-sdk-fixture-only',NEXUS_ADVERTISE_ADDR=f'127.0.0.1:{port}',NEXUS_CLUSTER_INIT='sharedzone',NEXUS_CLUSTER_INIT_MOUNTS='/agents=sharedzone',RUST_LOG='info')
process=None
start_offset=0
log=(run/'daemon.log').open('ab')
def start():
 global process,start_offset
 start_offset=(run/'daemon.log').stat().st_size
 process=subprocess.Popen([str(daemon),'--bind-addr',f'127.0.0.1:{port}'],env=env,stdout=log,stderr=subprocess.STDOUT)
 (run/'owned-daemon.json').write_text(json.dumps({'pid':process.pid,'binary':str(daemon),'data':str(data),'port':port}))
def stop():
 global process
 if process and process.poll() is None:
  process.terminate()
  try:process.wait(timeout=15)
  except subprocess.TimeoutExpired:process.kill();process.wait(timeout=10)
 process=None

def waitlog(needle):
 startAt=time.monotonic()
 while needle not in (run/'daemon.log').read_bytes()[start_offset:].decode('utf-8',errors='replace'):
  if process.poll() is not None:raise RuntimeError('Owned daemon exited')
  if time.monotonic()-startAt>45:raise TimeoutError(needle)
  time.sleep(.1)
try:
 start();waitlog('Static topology applied');stop()
 actors=[]
 for n in ['sender','receiver']:
  name='acp-sdk-'+n+'-'+run.name[4:]
  mint=subprocess.run([str(daemon),'auth','mint','--subject-type','agent','--subject-id',name,'--name','sdk-isolated'],env=env,capture_output=True)
  (run/(n+'-mint.log')).write_bytes(mint.stdout+mint.stderr)
  assert mint.returncode==0,mint.stderr.decode('utf-8',errors='replace')
  bundle=data/'agents'/name
  assert all((bundle/f).is_file() for f in ['credential.json','agent.pem','agent-key.pem','ca.pem']),str(bundle)
  home=run/(n+'-home');workspace=run/(n+'-workspace');configHome=run/(n+'-config')
  for p in [home,workspace,configHome]:p.mkdir()
  (workspace/'.nexus/sudocode').mkdir(parents=True);(workspace/'.nexus/sudocode/settings.json').write_text(json.dumps({'agentName':name}))
  (home/'.nexus/run').mkdir(parents=True);(home/'.nexus/run/node.json').write_text(json.dumps({'version':1,'endpoint':f'127.0.0.1:{port}','agents_dir':str(data/'agents'),'role':'founder','zone':'sharedzone'}))
  actorEnv={'HOME':str(home),'USERPROFILE':str(home),'SUDO_CODE_CONFIG_HOME':str(configHome),'NO_COLOR':'1','RUST_LOG':'info','APPDATA':str(home/'AppData/Roaming'),'LOCALAPPDATA':str(home/'AppData/Local')}
  for p in [Path(actorEnv['APPDATA']),Path(actorEnv['LOCALAPPDATA'])]:p.mkdir(parents=True)
  actors.append({'name':name,'workspace':str(workspace),'configHome':str(configHome),'env':actorEnv})
 # Installation uses only the generated canonical archive; no upper application.
 install=run/'install'
 p=subprocess.run([str(node),str(npm),'install','--offline','--ignore-scripts','--no-audit','--no-fund','--prefix',str(install),str(sdk/'artifacts/sudocode-acp-0.1.0.tgz')],env=env,capture_output=True)
 (run/'offline-install.log').write_bytes(p.stdout+p.stderr);assert p.returncode==0
 p=subprocess.run([str(node),str(install/'node_modules/@sudocode/acp/dist/server.cjs'),'--verify'],env=env,capture_output=True);assert p.returncode==0,p.stderr
 start();waitlog('VFS data plane ready')
 config={'root':str(run),'install':str(install),'sdkSource':str(sdk),'scode':str(scode),'scodeSha256':scodeReceipt['binarySha256'],'source':scodeReceipt['sourceCommit'],'packageSource':json.loads((sdk/'manifest.json').read_text())['sourceCommit'],'sampleConfig':str(sdk/'../../rust/crates/runtime/src/sudocode.sample.json'),'actors':actors,'marker':uuid.uuid4().hex,'daemonReceipt':daemonReceipt}
 cfg=run/'driver.json';cfg.write_text(json.dumps(config))
 p=subprocess.run([str(node),str(sdk/'test/nativeIntegration.cjs'),str(cfg)],env=env,capture_output=True,timeout=180)
 (run/'test.stdout').write_bytes(p.stdout);(run/'test.stderr').write_bytes(p.stderr)
 print(p.stdout.decode('utf-8',errors='replace'));print(p.stderr.decode('utf-8',errors='replace'))
 assert p.returncode==0,'Native paired process test failed'
finally:
 stop();log.close()
 (root/'latest-run.json').write_text(json.dumps({'run':str(run),'daemonStopped':True,'port':port},indent=2)+'\n')
 # Keys and database state belong solely to this throwaway fixture.
 for child in [data,identity,*[run/(n+suffix) for n in ['sender','receiver'] for suffix in ['-home','-workspace','-config']]]:
  child=child.resolve()
  assert child.parent==run.resolve() and child!=run.resolve()
  if child.exists():shutil.rmtree(child)
 print('Evidence directory:',run)
