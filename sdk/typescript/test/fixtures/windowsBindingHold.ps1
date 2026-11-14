param([Parameter(Mandatory=$true)][string]$BindingPath)
$ErrorActionPreference = 'Stop'
# A real Windows handle allows reads/writes but denies delete/rename sharing.
# Only a private fixture binding is supplied by the integration process.
$bindingStream = [System.IO.File]::Open($BindingPath, [System.IO.FileMode]::Open,
    [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
try {
    [Console]::Out.WriteLine('held')
    [Console]::Out.Flush()
    $releaseMode = [Console]::In.ReadLine()
    if ($releaseMode -eq 'delayed') { Start-Sleep -Milliseconds 250 }
} finally {
    $bindingStream.Dispose()
}
