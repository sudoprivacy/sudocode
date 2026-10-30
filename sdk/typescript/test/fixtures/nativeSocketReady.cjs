const fs=require('node:fs'),path=require('node:path');

// Windows AF_UNIX entries cannot reliably be stat'ed. This only checks the
// directory entry; an actual protocol connection is still needed for readiness.
module.exports=file=>{
  try{return fs.readdirSync(path.dirname(file)).includes(path.basename(file));}
  catch(error){if(error.code==='ENOENT')return false;throw error;}
};
