param(
    [ValidateSet('VerifyProcess', 'ReadDirectoryAcl', 'DenyDirectoryRead', 'RestoreDirectoryRead')]
    [string]$Operation,
    [string]$Target,
    [string]$Value,
    [string]$Executable
)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)

if ($Operation -eq 'VerifyProcess') {
    $RuntimeId = [uint32]$Target
    $TransportId = [uint32]$Value
    $RuntimeProcess = Get-CimInstance Win32_Process -Filter "ProcessId=$RuntimeId"
    $TransportProcess = Get-CimInstance Win32_Process -Filter "ProcessId=$TransportId"
    if (!$RuntimeProcess -or !$TransportProcess) { throw 'Owned runtime or PTY has exited' }
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
Write-Output $Acl.GetSecurityDescriptorSddlForm($Access)
