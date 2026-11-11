param(
    [ValidateSet('VerifyProcess', 'SetReadBypass', 'ReadDirectoryAcl', 'DenyDirectoryRead', 'RestoreDirectoryRead')]
    [string]$Operation,
    [string]$Target,
    [string]$Value,
    [string]$Executable,
    [string]$ExpectedCreationTicks,
    [ValidateSet('Disabled', 'Enabled')]
    [string]$ReadBypass = 'Disabled'
)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
# These native fixture controls use only this executable's builtin modules.
# Intermediate Python/Rust parents can retain PowerShell 7 module paths.
$env:PSModulePath = [System.IO.Path]::Combine($PSHOME, 'Modules')

if ($Operation -eq 'VerifyProcess' -or $Operation -eq 'SetReadBypass') {
    $RuntimeId = [uint32]$Target
    $TransportId = [uint32]$Value
    $RuntimeProcess = Get-CimInstance Win32_Process -Filter "ProcessId=$RuntimeId"
    $TransportProcess = Get-CimInstance Win32_Process -Filter "ProcessId=$TransportId"
    if (!$RuntimeProcess -or !$TransportProcess) { throw 'Owned runtime or PTY has exited' }
    $Owner = Invoke-CimMethod -InputObject $RuntimeProcess -MethodName GetOwnerSid
    $CurrentSid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    if ($Owner.ReturnValue -ne 0 -or $Owner.Sid -ne $CurrentSid) {
        throw 'Owned runtime does not share the native fixture security principal'
    }
    $ActualPath = [System.IO.Path]::GetFullPath($RuntimeProcess.ExecutablePath)
    $ExpectedPath = [System.IO.Path]::GetFullPath($Executable)
    if (![String]::Equals($ActualPath, $ExpectedPath, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Reported runtime is not the selected executable'
    }
    $Cursor = $RuntimeProcess
    $Seen = [System.Collections.Generic.HashSet[uint32]]::new()
    while ($Cursor.ProcessId -ne $TransportId) {
        if (!$Seen.Add($Cursor.ProcessId) -or $Seen.Count -gt 64 -or
            $Cursor.CreationDate -lt $TransportProcess.CreationDate) {
            throw 'Reported runtime is not a current descendant of the owned PTY'
        }
        $Parent = Get-CimInstance Win32_Process -Filter "ProcessId=$($Cursor.ParentProcessId)"
        if (!$Parent -or $Parent.CreationDate -gt $Cursor.CreationDate) {
            throw 'Native runtime ancestry is unavailable or contains a reused PID'
        }
        $Cursor = $Parent
    }
    if ($Operation -eq 'SetReadBypass') {
        # Git Bash enables this privilege for administrator tokens before
        # launching native children. An ACL fault must also bind the reader's
        # effective access, rather than only the parent test process's token.
        Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
public static class ScodeFixtureReadPrivilege {
    [StructLayout(LayoutKind.Sequential, Pack = 4)]
    struct Luid { public uint Low; public int High; }
    [StructLayout(LayoutKind.Sequential, Pack = 4)]
    struct TokenPrivileges { public uint Count; public Luid Id; public uint Attributes; }
    public sealed class State { public bool Present; public bool Enabled; public string CreationTicks; }
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern IntPtr OpenProcess(uint access, bool inherit, uint pid);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool CloseHandle(IntPtr handle);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool GetProcessTimes(IntPtr process, out long creation, out long exit, out long kernel, out long user);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool OpenProcessToken(IntPtr process, uint access, out IntPtr token);
    [DllImport("advapi32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool LookupPrivilegeValue(string system, string name, out Luid id);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool GetTokenInformation(IntPtr token, uint kind, IntPtr data, uint length, out uint needed);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool AdjustTokenPrivileges(IntPtr token, bool disableAll, ref TokenPrivileges state,
        uint length, IntPtr previous, IntPtr needed);
    static State Read(IntPtr token, Luid id) {
        uint length;
        GetTokenInformation(token, 3, IntPtr.Zero, 0, out length);
        if (Marshal.GetLastWin32Error() != 122 || length < 4 || length > 1048576)
            throw new Win32Exception(Marshal.GetLastWin32Error(), "Query fixture token size");
        IntPtr data = Marshal.AllocHGlobal(checked((int)length));
        try {
            if (!GetTokenInformation(token, 3, data, length, out length))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "Read fixture token");
            int count = Marshal.ReadInt32(data);
            if (count < 0 || 4L + 12L * count > length)
                throw new InvalidOperationException("Invalid native privilege inventory");
            for (int i = 0; i < count; ++i) {
                int offset = 4 + 12 * i;
                if (unchecked((uint)Marshal.ReadInt32(data, offset)) == id.Low &&
                    Marshal.ReadInt32(data, offset + 4) == id.High)
                    return new State { Present = true, Enabled = (Marshal.ReadInt32(data, offset + 8) & 2) != 0 };
            }
            return new State();
        } finally { Marshal.FreeHGlobal(data); }
    }
    public static State Set(uint pid, bool enabled, long observedTicks, string expectedTicks) {
        IntPtr process = OpenProcess(0x1000, false, pid);
        if (process == IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error(), "Open owned fixture");
        IntPtr token = IntPtr.Zero;
        try {
            long creation, exit, kernel, user;
            if (!GetProcessTimes(process, out creation, out exit, out kernel, out user))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "Read owned fixture birth");
            string ticks = DateTime.FromFileTimeUtc(creation).Ticks.ToString();
            // CIM has microsecond precision; the pinned native handle retains
            // the exact 100 ns birth time for every subsequent restoration.
            if (DateTime.FromFileTimeUtc(creation).Ticks / 10 != observedTicks / 10 ||
                (!String.IsNullOrEmpty(expectedTicks) && expectedTicks != ticks))
                throw new InvalidOperationException("Owned fixture PID has been reused");
            if (!OpenProcessToken(process, 0x28, out token))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "Open owned fixture token");
            Luid id;
            if (!LookupPrivilegeValue(null, "SeBackupPrivilege", out id))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "Resolve backup privilege");
            State before = Read(token, id);
            if (before.Present) {
                TokenPrivileges desired = new TokenPrivileges { Count = 1, Id = id, Attributes = enabled ? 2U : 0U };
                if (!AdjustTokenPrivileges(token, false, ref desired, 0, IntPtr.Zero, IntPtr.Zero))
                    throw new Win32Exception(Marshal.GetLastWin32Error(), "Change owned fixture read privilege");
                int error = Marshal.GetLastWin32Error();
                if (error != 0) throw new Win32Exception(error, "Assign owned fixture read privilege");
            } else if (enabled) {
                throw new InvalidOperationException("Cannot add a missing fixture privilege");
            }
            State after = Read(token, id);
            if (after.Present != before.Present || (after.Present && after.Enabled != enabled))
                throw new InvalidOperationException("Native privilege change did not hold");
            before.CreationTicks = ticks;
            return before;
        } finally {
            if (token != IntPtr.Zero) CloseHandle(token);
            CloseHandle(process);
        }
    }
}
'@
        $Before = [ScodeFixtureReadPrivilege]::Set(
            $RuntimeId, $ReadBypass -eq 'Enabled',
            $RuntimeProcess.CreationDate.ToUniversalTime().Ticks, $ExpectedCreationTicks
        )
        [ordered]@{
            pid = $RuntimeId
            creation_ticks = $Before.CreationTicks
            present = $Before.Present
            was_enabled = $Before.Enabled
            enabled = ($Before.Present -and $ReadBypass -eq 'Enabled')
        } | ConvertTo-Json -Compress
        exit 0
    }
    Write-Output $RuntimeId
    exit 0
}

$Acl = Get-Acl -LiteralPath $Target
$Access = [System.Security.AccessControl.AccessControlSections]::Access
if ($Operation -eq 'DenyDirectoryRead') {
    $Identity = [System.Security.Principal.WindowsIdentity]::GetCurrent().User
    $Rule = [System.Security.AccessControl.FileSystemAccessRule]::new(
        $Identity,
        [System.Security.AccessControl.FileSystemRights]::ListDirectory,
        [System.Security.AccessControl.InheritanceFlags]::None,
        [System.Security.AccessControl.PropagationFlags]::None,
        [System.Security.AccessControl.AccessControlType]::Deny
    )
    $Acl.AddAccessRule($Rule)
    Set-Acl -LiteralPath $Target -AclObject $Acl
} elseif ($Operation -eq 'RestoreDirectoryRead') {
    $Acl.SetSecurityDescriptorSddlForm($Value, $Access)
    Set-Acl -LiteralPath $Target -AclObject $Acl
    $Acl = Get-Acl -LiteralPath $Target
}
# Match native policy, not incidental SDDL auto-inheritance bookkeeping. Windows
# may set SE_DACL_AUTO_INHERITED after Set-Acl. Keep every other control bit and
# exact ordered ACE bytes, including masks, SIDs and per-ACE inheritance flags.
$Raw = [System.Security.AccessControl.RawSecurityDescriptor]::new(
    $Acl.GetSecurityDescriptorBinaryForm(), 0
)
$AceValues = $null
$Revision = $null
if ($null -ne $Raw.DiscretionaryAcl) {
    $Revision = [int]$Raw.DiscretionaryAcl.Revision
    $AceValues = @(
        foreach ($Ace in $Raw.DiscretionaryAcl) {
            $Bytes = [byte[]]::new($Ace.BinaryLength)
            $Ace.GetBinaryForm($Bytes, 0)
            [Convert]::ToBase64String($Bytes)
        }
    )
}
[ordered]@{
    caller_sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    target = $Target
    descriptor = $Acl.GetSecurityDescriptorSddlForm($Access)
    policy = [ordered]@{
        owner = $Raw.Owner.Value
        group = $Raw.Group.Value
        control_flags = ([int]$Raw.ControlFlags -band (-bnot 0x0400))
        dacl_revision = $Revision
        aces = $AceValues
    }
} | ConvertTo-Json -Depth 4 -Compress
