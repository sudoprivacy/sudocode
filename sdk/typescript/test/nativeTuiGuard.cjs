// Real PTY/ConPTY processes: a close request must never confirm an installer.
const assert=require('node:assert/strict'),fs=require('node:fs'),os=require('node:os'),path=require('node:path');
const {spawnSync}=require('node:child_process');
const python=process.env.SCODE_TEST_PYTHON||(process.platform==='win32'?'python':'python3');
const driver=path.join(__dirname,'fixtures',process.platform==='win32'?'nativeCodexTui.windows.py':'nativeCodexTui.py');
const root=fs.mkdtempSync(path.join(os.tmpdir(),'native-tui-guard-'));
const fake=String.raw`
import os,sys
source=open('CONIN$') if os.name=='nt' else sys.stdin
output=open('CONOUT$','w') if os.name=='nt' else sys.stdout
unsafe=sys.argv[1]=='upgrade'
print('Update available\n› 1. Update now\n  2. Skip' if unsafe else '› Ready',file=output,flush=True)
line=source.readline()
if unsafe:
    open(sys.argv[2],'w').write('INSTALLER_CONFIRMATION_RECEIVED')
else:
    assert line.strip()=='/exit',repr(line)
`;
try{
  for(const mode of ['normal','upgrade']){
    const marker=path.join(root,mode+'.marker');
    const result=spawnSync(python,[driver,python,'-u','-c',fake,mode,marker],{
      input:JSON.stringify({action:'close'})+'\n',encoding:'utf8',timeout:12000,
      env:{...process.env,NATIVE_LIFECYCLE_PTY_TRACE:path.join(root,mode+'.json')},
    });
    const events=(result.stdout||'').trim().split('\n').filter(Boolean).map(line=>JSON.parse(line));
    if(result.error){
      for(const event of events.filter(row=>row.event==='spawn')){
        try{process.kill(event.pid,'SIGKILL');}catch(error){if(error.code!=='ESRCH')throw error;}
      }
      throw result.error;
    }
    assert.equal(result.status,0,result.stderr);
    if(mode==='normal'){
      assert(events.some(row=>row.event==='quit_sent'),JSON.stringify(events));
      assert.equal(events.find(row=>row.event==='exit')?.returncode,0,JSON.stringify(events));
      assert(!events.some(row=>['unsafe_update_prompt','terminate','kill','normal_exit_timeout'].includes(row.event)));
    }else{
      assert(events.some(row=>row.event==='unsafe_update_prompt'),JSON.stringify(events));
      assert(!events.some(row=>row.event==='quit_sent'),JSON.stringify(events));
      assert(!fs.existsSync(marker),'The mock installer must receive no confirmation');
    }
  }
  console.log('Native TUI guard PASS: normal /exit; upgrade dialog refused without confirmation');
}finally{fs.rmSync(root,{recursive:true,force:true});}
