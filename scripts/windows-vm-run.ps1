[CmdletBinding()]
param(
    [Parameter(Mandatory)] [string] $Archive,
    [switch] $CheckOnly
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

if (-not $IsWindows -or $PSVersionTable.PSEdition -ne "Core") {
    throw "scripts/windows-vm-run.ps1 requires native Windows and PowerShell 7"
}

function Invoke-Checked {
    param(
        [Parameter(Mandatory)] [string] $Program,
        [Parameter(ValueFromRemainingArguments)] [string[]] $NativeArgs
    )

    & $Program @NativeArgs
    if ($LASTEXITCODE -ne 0) {
        throw "$Program exited with status $LASTEXITCODE"
    }
}

function Resolve-RequiredCommand {
    param([Parameter(Mandatory)] [string] $Name)

    $command = Get-Command $Name -ErrorAction Stop | Select-Object -First 1
    return $command.Source
}

function Enter-VisualStudioEnvironment {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path -LiteralPath $vswhere -PathType Leaf)) {
        throw "Visual Studio locator is missing: $vswhere"
    }
    $installation = (& $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath).Trim()
    if ([string]::IsNullOrWhiteSpace($installation)) {
        throw "Visual Studio C++ Build Tools workload is unavailable"
    }
    $module = Join-Path $installation "Common7\Tools\Microsoft.VisualStudio.DevShell.dll"
    Import-Module $module -ErrorAction Stop
    Enter-VsDevShell `
        -VsInstallPath $installation `
        -SkipAutomaticLocation `
        -DevCmdArguments "-arch=x64 -host_arch=x64"
}

$machinePath = [Environment]::GetEnvironmentVariable("Path", "Machine")
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
$env:Path = @($machinePath, $userPath) -join ";"
Enter-VisualStudioEnvironment

$vmRoot = "C:\PrismVm"
$sourceRoot = Join-Path $vmRoot "source"
$targetRoot = Join-Path $sourceRoot "target"
$toolsRoot = Join-Path $vmRoot "tools"
$resolvedArchive = (Resolve-Path -LiteralPath $Archive).Path

New-Item -ItemType Directory -Path $sourceRoot -Force | Out-Null
if (Test-Path -LiteralPath $targetRoot) {
    $target = Get-Item -LiteralPath $targetRoot -Force
    if (-not $target.PSIsContainer -or ($target.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw "persistent target path is not a real directory: $targetRoot"
    }
}
else {
    New-Item -ItemType Directory -Path $targetRoot | Out-Null
}

Get-ChildItem -LiteralPath $sourceRoot -Force |
    Where-Object { $_.Name -ne "target" } |
    Remove-Item -Recurse -Force

Write-Host "==> Extract current Linux worktree into $sourceRoot"
Invoke-Checked tar.exe "-xf" $resolvedArchive "-C" $sourceRoot
Remove-Item -LiteralPath $resolvedArchive -Force

$psmux = Join-Path $toolsRoot "psmux.exe"
$worktrunk = Join-Path $toolsRoot "git-wt.exe"
$opencode = Resolve-RequiredCommand "opencode.cmd"
$env:PRISM_TEST_PSMUX = $psmux
$env:PRISM_TEST_TMUX = $psmux
$env:PRISM_TEST_WORKTRUNK = $worktrunk
$env:PRISM_TEST_OPENCODE = $opencode

foreach ($tool in @($psmux, $worktrunk, $opencode)) {
    if (-not (Test-Path -LiteralPath $tool -PathType Leaf)) {
        throw "required Windows smoke tool is missing: $tool"
    }
}
Resolve-RequiredCommand "git.exe" | Out-Null
Resolve-RequiredCommand "cargo.exe" | Out-Null
Resolve-RequiredCommand "link.exe" | Out-Null

Push-Location $sourceRoot
try {
    Write-Host "==> Native Windows gate"
    & (Join-Path $sourceRoot "scripts\windows-check.ps1")
    if ($LASTEXITCODE -ne 0) {
        throw "scripts/windows-check.ps1 exited with status $LASTEXITCODE"
    }

    if (-not $CheckOnly) {
        Write-Host "==> Native Windows Git, Worktrunk, OpenCode, and psmux smoke"
        & (Join-Path $sourceRoot "scripts\windows-platform-smoke.ps1")
        if ($LASTEXITCODE -ne 0) {
            throw "scripts/windows-platform-smoke.ps1 exited with status $LASTEXITCODE"
        }
    }

    Write-Host "Windows VM tests PASS"
}
catch {
    Write-Error "Windows VM tests stopped at the first failure. Source retained at ${sourceRoot}: $($_.Exception.Message)"
    exit 1
}
finally {
    Pop-Location
}
