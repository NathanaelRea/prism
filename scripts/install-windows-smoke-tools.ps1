[CmdletBinding()]
param(
    [string] $Destination
)

$ErrorActionPreference = "Stop"
if (-not $IsWindows -or $PSVersionTable.PSEdition -ne "Core") {
    throw "Windows smoke tools require native Windows and PowerShell 7"
}
if ([string]::IsNullOrWhiteSpace($Destination)) {
    $destinationBase = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [System.IO.Path]::GetTempPath() }
    $Destination = Join-Path $destinationBase "prism-windows-tools"
}

$psmuxVersion = "3.3.7"
$psmuxSha256 = "60ff7b236f64184921cef3c1ff2611aa5a36fcc7ed8e2a58e968b8ded57f6028"
$worktrunkVersion = "0.71.0"
$worktrunkSha256 = "3af1357199574a13852931eee5b2b11f5027c6c3bd7cf99d9d4cd36cd838a6e3"

function Install-ZipTool {
    param(
        [string] $Name,
        [string] $Url,
        [string] $Sha256,
        [string] $Executable,
        [string] $FallbackExecutable
    )
    $archive = Join-Path $Destination "$Name.zip"
    $expanded = Join-Path $Destination "$Name-expanded"
    Invoke-WebRequest -Uri $Url -OutFile $archive
    $actual = (Get-FileHash -Algorithm SHA256 $archive).Hash.ToLowerInvariant()
    if ($actual -ne $Sha256) {
        throw "$Name checksum mismatch: expected $Sha256, got $actual"
    }
    Expand-Archive -Path $archive -DestinationPath $expanded
    $source = Get-ChildItem -Path $expanded -Filter $Executable -File -Recurse | Select-Object -First 1
    if ($null -eq $source -and $FallbackExecutable) {
        $source = Get-ChildItem -Path $expanded -Filter $FallbackExecutable -File -Recurse | Select-Object -First 1
    }
    if ($null -eq $source) { throw "$Name archive omitted $Executable" }
    Copy-Item $source.FullName (Join-Path $Destination $Executable)
}

Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $Destination
New-Item -ItemType Directory -Path $Destination | Out-Null
Install-ZipTool `
    -Name "psmux" `
    -Url "https://github.com/psmux/psmux/releases/download/v$psmuxVersion/psmux-v$psmuxVersion-windows-x64.zip" `
    -Sha256 $psmuxSha256 `
    -Executable "psmux.exe"
Install-ZipTool `
    -Name "worktrunk" `
    -Url "https://github.com/max-sixty/worktrunk/releases/download/v$worktrunkVersion/worktrunk-x86_64-pc-windows-msvc.zip" `
    -Sha256 $worktrunkSha256 `
    -Executable "git-wt.exe" `
    -FallbackExecutable "wt.exe"

$psmux = Join-Path $Destination "psmux.exe"
$worktrunk = Join-Path $Destination "git-wt.exe"
& $psmux --version
if ($LASTEXITCODE -ne 0) { throw "psmux --version failed" }
& $worktrunk --version
if ($LASTEXITCODE -ne 0) { throw "git-wt --version failed" }

$env:Path = "$Destination;$env:Path"
$env:PRISM_TEST_PSMUX = $psmux
$env:PRISM_TEST_TMUX = $psmux
$env:PRISM_TEST_WORKTRUNK = $worktrunk
if ($env:GITHUB_PATH) {
    Add-Content -LiteralPath $env:GITHUB_PATH -Value $Destination
}
if ($env:GITHUB_ENV) {
    Add-Content -LiteralPath $env:GITHUB_ENV -Value "PRISM_TEST_PSMUX=$psmux"
    Add-Content -LiteralPath $env:GITHUB_ENV -Value "PRISM_TEST_TMUX=$psmux"
    Add-Content -LiteralPath $env:GITHUB_ENV -Value "PRISM_TEST_WORKTRUNK=$worktrunk"
}
