import { execFile, execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { readFileSync, realpathSync, statSync, writeFileSync } from 'node:fs';
import * as path from 'node:path';
import { promisify } from 'node:util';

const execute = promisify(execFile);

function systemExecutable(name: string): string {
  const root = process.env.SystemRoot;
  if (process.platform !== 'win32' || !root || !path.isAbsolute(root)) {
    throw new Error('Windows owner preparation requires the current Windows user environment');
  }
  const executable = path.join(root, 'System32', name);
  if (!statSync(executable).isFile()) throw new Error(`Missing Windows system executable: ${name}`);
  return executable;
}

/** The same canonical private config selects one task for preparation and use. */
function taskName(file: string): string {
  return 'SudocodeCodexOwner_' + createHash('sha256').update(realpathSync.native(file)).digest('hex').slice(0, 24);
}

function xml(value: string): string {
  return value.replace(/[&<>"']/g, character => ({'&':'&amp;', '<':'&lt;', '>':'&gt;', '"':'&quot;', "'":'&apos;'}[character]!));
}

function windowsArgument(value: string): string {
  // Task Scheduler expands environment placeholders in Exec fields. A literal
  // percent must not silently resolve a selected path to another command.
  if (/[\0\r\n%]/.test(value)) throw new Error('Owner action paths cannot contain control characters or percent placeholders');
  return '"' + value.replace(/(\\*)"/g, (_match, slashes: string) => slashes + slashes + '\\"')
    .replace(/\\+$/, slashes => slashes + slashes) + '"';
}

export interface WindowsCodexOwnerPreparation {
  prepared: true;
  registered: false;
  started: false;
  taskName: string;
  taskFile: string;
  taskSha256: string;
  installationArgv: string[];
  removalArgv: string[];
}

/** Prepare an on-demand, current-user action. Registration is an explicit
 * operator step; this never installs a task or starts an owner/native server.
 * Task Scheduler, rather than the first MCP's kill-on-close Job, launches the
 * SAME canonical owner. No password, elevation, trigger or Job escape is used.
 */
export function prepareWindowsOwnerTask(file: string, entry: string): WindowsCodexOwnerPreparation {
  const scheduler = systemExecutable('schtasks.exe');
  if (![file, entry, process.execPath].every(value => path.isAbsolute(value) && statSync(value).isFile())) {
    throw new Error('Existing absolute private config, canonical entry and Node executable are required');
  }
  const identity = execFileSync(systemExecutable('whoami.exe'), ['/user', '/fo', 'csv', '/nh'],
    {encoding:'utf8', windowsHide:true, timeout:3000, maxBuffer:16384});
  const sids = identity.match(/S-1-\d+(?:-\d+)+/g);
  if (sids?.length !== 1) throw new Error('Could not identify the current Windows user SID');
  const name = taskName(file);
  const taskFile = file + '.windows-owner.xml';
  windowsArgument(process.execPath);
  const argumentsText = [entry, '--config', file, '--codex-owner'].map(windowsArgument).join(' ');
  // InteractiveToken uses the already signed-in user's token without storing a
  // password. IgnoreNew plus the existing exclusive endpoint prevents duplicate
  // owners. The existing root inventory and idle timer still decide retirement.
  const definition = `<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.3" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>Sudocode Codex A2A owner for one private configuration</Description></RegistrationInfo>
  <Principals><Principal id="Owner"><UserId>${xml(sids[0])}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
  <Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><AllowStartOnDemand>true</AllowStartOnDemand><Enabled>true</Enabled><ExecutionTimeLimit>PT0S</ExecutionTimeLimit></Settings>
  <Actions Context="Owner"><Exec><Command>${xml(process.execPath)}</Command><Arguments>${xml(argumentsText)}</Arguments><WorkingDirectory>${xml(path.dirname(file))}</WorkingDirectory></Exec></Actions>
</Task>
`;
  const bytes = Buffer.concat([Buffer.from([0xff, 0xfe]), Buffer.from(definition, 'utf16le')]);
  try {writeFileSync(taskFile, bytes, {flag:'wx', mode:0o600});}
  catch(error){
    if((error as NodeJS.ErrnoException).code!=='EEXIST')throw error;
    if(!statSync(taskFile).isFile() || !readFileSync(taskFile).equals(bytes)){
      throw new Error('Existing Windows owner task plan differs; prepare a new versioned private configuration');
    }
  }
  return {prepared:true, registered:false, started:false, taskName:name, taskFile,
    taskSha256:createHash('sha256').update(bytes).digest('hex'),
    installationArgv:[scheduler, '/Create', '/TN', name, '/XML', taskFile],
    removalArgv:[scheduler, '/Delete', '/TN', name, '/F']};
}

/** Request the previously registered action once. A scheduler reply is never
 * an owner-ready receipt; callers still authenticate the real exclusive socket.
 */
export async function startWindowsOwnerTask(file: string, deadline: number): Promise<void> {
  try {
    await execute(systemExecutable('schtasks.exe'), ['/Run', '/TN', taskName(file)],
      {windowsHide:true, timeout:Math.max(1, deadline - Date.now()), maxBuffer:65536});
  } catch (error) {
    throw new Error('Windows Codex A2A owner did not start through its prepared task; use --prepare-codex-owner and register the reviewed action', {cause:error});
  }
}
