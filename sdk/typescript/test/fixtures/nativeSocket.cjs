const fs=require('node:fs'),path=require('node:path');

// Windows AF_UNIX entries cannot reliably be stat'ed. This only checks the
// directory entry; an actual protocol connection is still needed for readiness.
const socketEntryPresent=file=>{
  try{return fs.readdirSync(path.dirname(file)).includes(path.basename(file));}
  catch(error){if(error.code==='ENOENT')return false;throw error;}
};

// Compare endpoint addresses through their real parent directory and exact
// entry name. Windows AF_UNIX leaves are reparse points that Node cannot
// realpath; parent errors still propagate. This does not prove a listener is
// serving: callers must connect through the actual native protocol separately.
const canonicalSocketPath=file=>path.join(fs.realpathSync(path.dirname(file)),path.basename(file));

module.exports={socketEntryPresent,canonicalSocketPath};
