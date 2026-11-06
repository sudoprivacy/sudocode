param(
    [ValidateSet('X64', 'ARM64')][string]$Architecture = $env:RUNNER_ARCH,
    [string]$EnvironmentFile = $env:GITHUB_ENV
)

$ErrorActionPreference = 'Stop'
if (-not $EnvironmentFile) { throw 'Pass EnvironmentFile or run this step in GitHub Actions' }
if (-not $Architecture) { $Architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString().ToUpperInvariant() }
$nativeArchitecture = if ($Architecture -eq 'ARM64') { 'arm64' } else { 'x64' }
$compilerBatch = git config --get localAcceptance.vsdevcmd
if (-not $compilerBatch) {
    $locator = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
    $component = if ($Architecture -eq 'ARM64') { 'Microsoft.VisualStudio.Component.VC.Tools.ARM64' } else { 'Microsoft.VisualStudio.Component.VC.Tools.x86.x64' }
    $installation = & $locator -latest -products '*' -requires $component -property installationPath
    $compilerBatch = Join-Path $installation 'Common7/Tools/VsDevCmd.bat'
}
if (-not (Test-Path -LiteralPath $compilerBatch -PathType Leaf)) { throw 'Native MSVC tools are missing; install the target architecture C++ tools before release acceptance' }
$invocation = '""{0}" -no_logo -arch={1} -host_arch={1} >nul && set"' -f $compilerBatch, $nativeArchitecture
$settings = & cmd.exe /d /s /c $invocation
if ($LASTEXITCODE -ne 0) { throw 'Native MSVC environment setup failed; release acceptance has not started' }
$allowed = @('PATH', 'LIB', 'LIBPATH', 'INCLUDE', 'VCToolsInstallDir', 'VCINSTALLDIR', 'VSINSTALLDIR', 'WindowsSdkDir', 'WindowsSDKVersion', 'VSCMD_ARG_HOST_ARCH', 'VSCMD_ARG_TGT_ARCH')
$exported = @{}
foreach ($line in $settings) {
    $parts = $line -split '=', 2
    if ($parts.Count -eq 2 -and $parts[0] -in $allowed) { $exported[$parts[0]] = $parts[1] }
}
if (-not $exported['LIB'] -or -not $exported['INCLUDE'] -or -not $exported['PATH']) { throw 'Native MSVC environment is incomplete; release acceptance has not started' }
foreach ($name in $exported.Keys) { "$name=$($exported[$name])" | Out-File -LiteralPath $EnvironmentFile -Append -Encoding utf8 }
Write-Output "RELEASE_MSVC_TARGET=$nativeArchitecture; native compiler environment prepared"
