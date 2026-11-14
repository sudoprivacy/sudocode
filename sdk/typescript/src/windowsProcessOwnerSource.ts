/** One packaged Windows adapter, used for both capture and lifetime monitoring.
 * Only SYNCHRONIZE | QUERY_LIMITED_INFORMATION are requested. No PID polling,
 * process termination, registry/task installation or execution-policy change.
 */
export const windowsProcessOwnerSource = String.raw`
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
[Console]::InputEncoding = [System.Text.UTF8Encoding]::new($false)
[Console]::WriteLine('{"phase":"bootstrap_started"}')
[Console]::Out.Flush()
# Resolve the same cmdlet before its unchanged invocation, so the original
# startup deadline can distinguish command discovery from compiler startup.
# This may warm discovery caches; reaching this marker is not qualification.
$bootstrapCommand = $ExecutionContext.InvokeCommand.GetCommand('Add-Type', [System.Management.Automation.CommandTypes]::Cmdlet)
if ($null -eq $bootstrapCommand) {
    [Console]::WriteLine('{"diagnostic":"bootstrap_command_lookup_returned","found":false}')
} else {
    [Console]::WriteLine('{"diagnostic":"bootstrap_command_lookup_returned","found":true}')
}
[Console]::Out.Flush()
Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
public sealed class ScodeProcessOwner : IDisposable {
    [StructLayout(LayoutKind.Sequential)] struct FT { public uint Low; public uint High; }
    [DllImport("kernel32.dll", SetLastError=true)] static extern IntPtr OpenProcess(uint rights, bool inherit, int pid);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool GetProcessTimes(IntPtr h, out FT c, out FT e, out FT k, out FT u);
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)] static extern bool QueryFullProcessImageName(IntPtr h, uint flags, StringBuilder name, ref uint size);
    [DllImport("kernel32.dll", SetLastError=true)] static extern uint WaitForSingleObject(IntPtr h, uint ms);
    [DllImport("kernel32.dll", SetLastError=true)] static extern uint WaitForMultipleObjects(uint n, IntPtr[] handles, bool all, uint ms);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr h);
    IntPtr handle;
    public readonly int Pid;
    public readonly string CreatedFiletime100ns;
    public readonly string Exe;
    public ScodeProcessOwner(int pid) {
        Pid = pid;
        handle = OpenProcess(0x00101000, false, pid);
        if (handle == IntPtr.Zero) throw new Win32Exception();
        try {
            FT c,e,k,u;
            if (!GetProcessTimes(handle, out c, out e, out k, out u)) throw new Win32Exception();
            CreatedFiletime100ns = (((ulong)c.High << 32) | c.Low).ToString(System.Globalization.CultureInfo.InvariantCulture);
            uint size = 32768; var text = new StringBuilder((int)size);
            if (!QueryFullProcessImageName(handle, 0, text, ref size)) throw new Win32Exception();
            Exe = text.ToString();
            RequireAlive();
        } catch { Dispose(); throw; }
    }
    public void RequireAlive() {
        uint result = WaitForSingleObject(handle, 0);
        if (result == 0) throw new InvalidOperationException("Selected process has exited");
        if (result != 258) throw new Win32Exception();
    }
    public string Wait(ScodeProcessOwner receiver) {
        // EOF belongs to this receiver's pipe. A background managed thread does
        // not run PowerShell callbacks and cannot keep the helper alive.
        var eof = new ManualResetEvent(false);
        var reader = new Thread(() => {
            try { while (Console.In.Read() != -1) {} }
            finally { eof.Set(); }
        });
        reader.IsBackground = true; reader.Start();
        uint result = WaitForMultipleObjects(3, new IntPtr[] {
            handle, receiver.handle, eof.SafeWaitHandle.DangerousGetHandle()
        }, false, UInt32.MaxValue);
        // If the reader is still blocked, process exit reclaims its event; do
        // not close an event which the EOF thread may subsequently signal.
        if (result == 2) { reader.Join(); eof.Dispose(); }
        if (result == 0) return "owner_exited";
        if (result == 1) return "receiver_exited";
        if (result == 2) return "channel_closed";
        throw new Win32Exception();
    }
    public void Dispose() {
        if (handle != IntPtr.Zero) { CloseHandle(handle); handle = IntPtr.Zero; }
    }
}
'@
[Console]::WriteLine('{"phase":"bootstrap_complete"}')
$selected = $null
$receiver = $null
try {
    $request = [Console]::In.ReadLine() | ConvertFrom-Json
    [Console]::WriteLine('{"phase":"request_received"}')
    $selected = [ScodeProcessOwner]::new([int]$request.pid)
    [Console]::WriteLine('{"phase":"owner_opened"}')
    $identity = @{platform='win32'; pid=$selected.Pid; createdFiletime100ns=$selected.CreatedFiletime100ns; exe=$selected.Exe}
    if ($request.mode -eq 'capture') {
        [Console]::WriteLine((@{phase='captured'; identity=$identity} | ConvertTo-Json -Compress))
    } elseif ($request.mode -eq 'watch') {
        if ($selected.CreatedFiletime100ns -cne $request.createdFiletime100ns -or $selected.Exe -cne $request.exe) {
            throw 'Selected process identity differs; do not adopt a replacement PID'
        }
        $receiver = [ScodeProcessOwner]::new([int]$request.receiverPid)
        [Console]::WriteLine('{"phase":"receiver_opened"}')
        $selected.RequireAlive()
        [Console]::WriteLine((@{phase='ready'; identity=$identity} | ConvertTo-Json -Compress))
        $reason = $selected.Wait($receiver)
        [Console]::WriteLine((@{phase='ended'; reason=$reason} | ConvertTo-Json -Compress))
    } else { throw 'Unknown owner adapter operation' }
} catch {
    [Console]::WriteLine((@{phase='error'; error=$_.Exception.Message} | ConvertTo-Json -Compress))
    exit 1
} finally {
    if ($null -ne $receiver) { $receiver.Dispose() }
    if ($null -ne $selected) { $selected.Dispose() }
}
`;
