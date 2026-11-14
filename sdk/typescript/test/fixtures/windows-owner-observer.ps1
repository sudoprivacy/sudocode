# Finite, read-only diagnosis of one test's new descendants. These WMI events
# and snapshots are never process custody, liveness, or cleanup authority.
param([uint32]$RootPid)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
$selected = [System.Collections.Generic.HashSet[uint32]]::new()
[void]$selected.Add($RootPid)
$count = 0
function Emit($value) {
    [Console]::WriteLine(($value | ConvertTo-Json -Compress -Depth 5))
    [Console]::Out.Flush()
}
try {
    foreach ($kind in @('Start', 'Stop')) {
        $null = Register-CimIndicationEvent -Namespace root/cimv2 `
            -Query "SELECT * FROM Win32_Process${kind}Trace" -SourceIdentifier "owner-$kind"
    }
    Emit @{kind='observer_ready'; pid=$PID; rootPid=$RootPid; at=[DateTime]::UtcNow.ToString('o')}
    # Read the pipe stream, not Console.In's synchronized TextReader (whose
    # ReadLineAsync can block synchronously on Windows PowerShell/.NET Framework).
    $stdinPipe = [Console]::OpenStandardInput()
    $inputByte = [byte[]]::new(1)
    $inputRead = $stdinPipe.ReadAsync($inputByte, 0, 1)
    $until = [DateTime]::UtcNow.AddSeconds(35)
    while ([DateTime]::UtcNow -lt $until -and $count -lt 256) {
        foreach ($event in @(Get-Event | Where-Object { $_.SourceIdentifier -like 'owner-*' })) {
            $row = $event.SourceEventArgs.NewEvent
            $processId = [uint32]$row.ProcessID
            $parentId = [uint32]$row.ParentProcessID
            if ($processId -ne $PID -and ($selected.Contains($processId) -or $selected.Contains($parentId))) {
                if ($event.SourceIdentifier -eq 'owner-Start') { [void]$selected.Add($processId) }
                Emit @{kind=$event.SourceIdentifier; pid=$processId; parentPid=$parentId;
                    name=[string]$row.ProcessName; nativeTimeCreated=[string]$row.TIME_CREATED;
                    receivedAt=[DateTime]::UtcNow.ToString('o'); exitStatus=$row.ExitStatus}
                $count += 1
            }
            Remove-Event -EventIdentifier $event.EventIdentifier
        }
        if ($inputRead.IsCompleted) {
            $readBytes = $inputRead.GetAwaiter().GetResult()
            if ($readBytes -eq 0 -or $inputByte[0] -eq 113) { break } # q or EOF
            if ($inputByte[0] -eq 115) { # s
                # At most the two explicit parent requests; no liveness polling.
                $rows = @(Get-CimInstance Win32_Process)
                do {
                    $changed = $false
                    foreach ($row in $rows) {
                        if ($row.ProcessId -ne $PID -and $selected.Contains([uint32]$row.ParentProcessId)) {
                            if ($selected.Add([uint32]$row.ProcessId)) { $changed = $true }
                        }
                    }
                } while ($changed)
                $owned = @($rows | Where-Object { $selected.Contains([uint32]$_.ProcessId) } |
                    Select-Object ProcessId,ParentProcessId,CreationDate,Name,ExecutablePath,KernelModeTime,UserModeTime)
                Emit @{kind='snapshot'; at=[DateTime]::UtcNow.ToString('o'); processes=$owned}
            }
            $inputRead = $stdinPipe.ReadAsync($inputByte, 0, 1)
        }
        # Native event wait, bounded so stdin stop/EOF is serviced as well.
        $null = Wait-Event -Timeout 1
    }
    Emit @{kind='observer_finished'; events=$count; boundedOut=($count -ge 256 -or [DateTime]::UtcNow -ge $until)}
} catch {
    Emit @{kind='observer_error'; error=[string]$_}
    exit 1
} finally {
    foreach ($id in @('owner-Start', 'owner-Stop')) {
        Unregister-Event -SourceIdentifier $id -ErrorAction SilentlyContinue
    }
}
