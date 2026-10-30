// Exercise the fixture's endpoint comparison on an actual AF_UNIX entry,
// including Windows' non-stat-able socket reparse point. No Codex is started.
const assert=require('node:assert/strict'),fs=require('node:fs'),os=require('node:os'),path=require('node:path');
const {spawnSync}=require('node:child_process');
const python=process.env.SCODE_TEST_PYTHON||(process.platform==='win32'?'python':'python3');
const root=fs.mkdtempSync(path.join(os.tmpdir(),'nsp-')),alias=root+'-alias';
const inspect=String.raw`
const assert=require('node:assert/strict'),fs=require('node:fs'),path=require('node:path');
const {socketEntryPresent,canonicalSocketPath}=require(process.argv[1]);
const root=process.argv[2],alias=process.argv[3],socket=path.join(root,'control.sock');
assert(socketEntryPresent(socket));
const canonical=canonicalSocketPath(socket);
assert.equal(canonical,path.join(fs.realpathSync(root),'control.sock'));
assert.equal(canonicalSocketPath(path.join(alias,'control.sock')),canonical,'canonical parent alias must match');
assert.notEqual(canonicalSocketPath(path.join(root,'other.sock')),canonical,'a different entry must not match');
fs.mkdirSync(path.join(root,'other'));
assert.notEqual(canonicalSocketPath(path.join(root,'other','control.sock')),canonical,'a different parent must not match');
assert(!socketEntryPresent(path.join(root,'missing','control.sock')));
assert.throws(()=>canonicalSocketPath(path.join(root,'missing','control.sock')),{code:'ENOENT'});
console.log('Native AF_UNIX path process PASS: real socket, parent alias, different endpoints and missing-parent refusal');
`;
try{
  fs.symlinkSync(root,alias,process.platform==='win32'?'junction':'dir');
  const result=spawnSync(python,[path.join(__dirname,'fixtures','nativeSocket.py'),process.execPath,inspect,
    path.join(__dirname,'fixtures','nativeSocket.cjs'),root,alias],
    {encoding:'utf8',timeout:12000});
  if(result.error)throw result.error;
  assert.equal(result.status,0,result.stdout+result.stderr);
  process.stdout.write(result.stdout);
}finally{
  fs.rmSync(alias,{recursive:true,force:true});
  fs.rmSync(root,{recursive:true,force:true});
}
