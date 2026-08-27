[CmdletBinding()]
param(
    [string] $Bridge = "\\tsclient\runner"
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$statusPath = Join-Path $Bridge "bootstrap.status"
$logPath = Join-Path $Bridge "bootstrap.log"
$publicKeyPath = Join-Path $Bridge "id_ed25519.pub"
$toolInstallerPath = Join-Path $Bridge "install-windows-smoke-tools.ps1"
$completionPath = "C:\PrismVm\bootstrap.complete"
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)

function Write-Status {
    param([Parameter(Mandatory)] [string] $Value)
    [System.IO.File]::WriteAllText($statusPath, "$Value`n", $utf8NoBom)
}

function Write-Log {
    param([Parameter(Mandatory)] [string] $Message)
    $line = "{0:o} {1}" -f [DateTime]::UtcNow, $Message
    Write-Host $line
    Add-Content -LiteralPath $logPath -Value $line -Encoding UTF8
}

function Invoke-Checked {
    param(
        [Parameter(Mandatory)] [string] $Program,
        [Parameter(ValueFromRemainingArguments)] [string[]] $NativeArgs
    )

    Write-Log ("Running: {0} {1}" -f $Program, ($NativeArgs -join " "))
    $oldErrorActionPreference = $ErrorActionPreference
    try {
        $ErrorActionPreference = "Continue"
        & $Program @NativeArgs 2>&1 | ForEach-Object {
            $outputLine = $_.ToString()
            if (-not [string]::IsNullOrEmpty($outputLine)) {
                Write-Log $outputLine
            }
        }
        $exitCode = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $oldErrorActionPreference
    }
    if ($exitCode -ne 0) {
        throw "$Program exited with status $exitCode"
    }
}

function Refresh-ProcessPath {
    $machinePath = [Environment]::GetEnvironmentVariable("Path", "Machine")
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $env:Path = @($machinePath, $userPath) -join ";"
}

function Add-UserPath {
    param([Parameter(Mandatory)] [string] $Directory)

    $current = [Environment]::GetEnvironmentVariable("Path", "User")
    $entries = @($current -split ";" | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
    if ($entries -notcontains $Directory) {
        $updated = @($entries + $Directory) -join ";"
        [Environment]::SetEnvironmentVariable("Path", $updated, "User")
    }
}

function Install-WingetPackage {
    param(
        [Parameter(Mandatory)] [string] $Id,
        [string] $Override
    )

    $winget = Get-Command winget.exe -ErrorAction Stop | Select-Object -First 1
    $arguments = @(
        "install",
        "--id", $Id,
        "--exact",
        "--silent",
        "--accept-package-agreements",
        "--accept-source-agreements",
        "--disable-interactivity"
    )
    if (-not [string]::IsNullOrWhiteSpace($Override)) {
        $arguments += @("--override", $Override)
    }
    Invoke-Checked $winget.Source @arguments
}

try {
    if (-not (Test-Path -LiteralPath $Bridge -PathType Container)) {
        throw "RDP bridge is unavailable: $Bridge"
    }
    [System.IO.File]::WriteAllText($logPath, "", $utf8NoBom)
    Remove-Item -LiteralPath $completionPath -Force -ErrorAction SilentlyContinue
    Write-Status "RUNNING preflight"

    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw "Windows VM bootstrap requires an elevated administrator session"
    }
    Write-Log "Bootstrap identity: $($identity.Name)"

    $drive = Get-PSDrive C
    $freeGb = [math]::Round($drive.Free / 1GB, 1)
    Write-Log "Free space on C: ${freeGb}GB"
    if ($freeGb -lt 25) {
        throw "Windows VM needs at least 25GB free for the native build toolchain"
    }

    if (-not (Test-Path -LiteralPath $publicKeyPath -PathType Leaf)) {
        throw "SSH public key is missing: $publicKeyPath"
    }
    $publicKey = (Get-Content -LiteralPath $publicKeyPath -Raw).Trim()
    if ($publicKey -match "[`r`n]" -or $publicKey -notmatch '^ssh-ed25519 [A-Za-z0-9+/]+={0,3}(?: [A-Za-z0-9._@+-]+)?$') {
        throw "SSH public key has an unexpected format"
    }

    Write-Status "RUNNING OpenSSH"
    $capability = Get-WindowsCapability -Online -Name "OpenSSH.Server*" | Select-Object -First 1
    if ($null -eq $capability -or $capability.State -ne "Installed") {
        Write-Log "Installing Windows OpenSSH Server capability"
        Add-WindowsCapability -Online -Name "OpenSSH.Server~~~~0.0.1.0" | Out-Null
        Write-Log "OpenSSH capability installation completed"
    }
    Set-Service -Name sshd -StartupType Automatic
    Start-Service -Name sshd

    $sshDirectory = Join-Path $env:ProgramData "ssh"
    $authorizedKeys = Join-Path $sshDirectory "administrators_authorized_keys"
    New-Item -ItemType Directory -Path $sshDirectory -Force | Out-Null
    $existingKeys = @()
    if (Test-Path -LiteralPath $authorizedKeys -PathType Leaf) {
        $existingKeys = @(Get-Content -LiteralPath $authorizedKeys | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
    }
    if ($existingKeys -notcontains $publicKey) {
        $existingKeys += $publicKey
    }
    [System.IO.File]::WriteAllLines($authorizedKeys, $existingKeys, $utf8NoBom)
    Invoke-Checked "$env:SystemRoot\System32\icacls.exe" $authorizedKeys "/inheritance:r" "/grant:r" "*S-1-5-18:F" "/grant:r" "*S-1-5-32-544:F"

    if (-not (Get-NetFirewallRule -Name "OpenSSH-Server-In-TCP" -ErrorAction SilentlyContinue)) {
        New-NetFirewallRule `
            -Name "OpenSSH-Server-In-TCP" `
            -DisplayName "OpenSSH Server (sshd)" `
            -Enabled True `
            -Direction Inbound `
            -Protocol TCP `
            -Action Allow `
            -LocalPort 22 | Out-Null
    }
    Restart-Service -Name sshd
    Write-Log "OpenSSH Server is running with the Prism VM host key authorized"

    Write-Status "RUNNING PowerShell"
    $powerShellCommand = Get-Command pwsh.exe -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($null -eq $powerShellCommand) {
        Install-WingetPackage "Microsoft.PowerShell"
        Refresh-ProcessPath
        $powerShellCommand = Get-Command pwsh.exe -ErrorAction Stop | Select-Object -First 1
    }
    $powerShell = $powerShellCommand.Source

    Write-Status "RUNNING Git"
    $git = Join-Path $env:ProgramFiles "Git\cmd\git.exe"
    if (-not (Test-Path -LiteralPath $git -PathType Leaf)) {
        Install-WingetPackage "Git.Git"
    }

    Write-Status "RUNNING Node"
    $nodeDirectory = Join-Path $env:ProgramFiles "nodejs"
    $node = Join-Path $nodeDirectory "node.exe"
    $installNode = -not (Test-Path -LiteralPath $node -PathType Leaf)
    if (-not $installNode) {
        $nodeVersion = (& $node --version).Trim()
        $installNode = $nodeVersion -notmatch '^v22\.'
    }
    if ($installNode) {
        Install-WingetPackage "OpenJS.NodeJS.22"
    }

    Write-Status "RUNNING Rust"
    $cargoDirectory = Join-Path $env:USERPROFILE ".cargo\bin"
    $rustup = Join-Path $cargoDirectory "rustup.exe"
    if (-not (Test-Path -LiteralPath $rustup -PathType Leaf)) {
        Install-WingetPackage "Rustlang.Rustup"
    }

    Write-Status "RUNNING Visual Studio Build Tools"
    $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    $vsInstallation = ""
    if (Test-Path -LiteralPath $vswhere -PathType Leaf) {
        $vsInstallation = (& $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath).Trim()
    }
    if ([string]::IsNullOrWhiteSpace($vsInstallation)) {
        Install-WingetPackage `
            "Microsoft.VisualStudio.2022.BuildTools" `
            "--wait --quiet --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
    }

    Refresh-ProcessPath
    Add-UserPath $cargoDirectory
    Add-UserPath $nodeDirectory
    Add-UserPath (Split-Path -Parent $powerShell)
    Add-UserPath (Split-Path -Parent $git)

    if (-not (Test-Path -LiteralPath $rustup -PathType Leaf)) {
        throw "rustup was not installed at $rustup"
    }
    Invoke-Checked $rustup "set" "profile" "minimal"
    Invoke-Checked $rustup "toolchain" "install" "stable-x86_64-pc-windows-msvc" "--component" "rustfmt" "--component" "clippy"
    Invoke-Checked $rustup "default" "stable-x86_64-pc-windows-msvc"

    Write-Status "RUNNING OpenCode"
    $npm = Join-Path $nodeDirectory "npm.cmd"
    if (-not (Test-Path -LiteralPath $npm -PathType Leaf)) {
        throw "npm was not installed at $npm"
    }
    Invoke-Checked $npm "install" "--global" "opencode-ai@1.17.20" "--no-audit" "--no-fund"
    $npmBin = Join-Path $env:APPDATA "npm"
    Add-UserPath $npmBin

    Write-Status "RUNNING smoke tools"
    if (-not (Test-Path -LiteralPath $toolInstallerPath -PathType Leaf)) {
        throw "Windows smoke-tool installer is missing: $toolInstallerPath"
    }
    $tools = "C:\PrismVm\tools"
    New-Item -ItemType Directory -Path (Split-Path -Parent $tools) -Force | Out-Null
    Invoke-Checked $powerShell "-NoLogo" "-NoProfile" "-ExecutionPolicy" "Bypass" "-File" $toolInstallerPath "-Destination" $tools
    Add-UserPath $tools

    Set-ItemProperty `
        -Path "HKLM:\SYSTEM\CurrentControlSet\Control\FileSystem" `
        -Name "LongPathsEnabled" `
        -Value 1
    Invoke-Checked $git "config" "--global" "core.longpaths" "true"

    Refresh-ProcessPath
    $vsInstallation = (& $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath).Trim()
    if ([string]::IsNullOrWhiteSpace($vsInstallation)) {
        throw "Visual Studio C++ Build Tools workload was not installed"
    }

    $requiredFiles = @(
        $powerShell,
        $git,
        $node,
        (Join-Path $cargoDirectory "cargo.exe"),
        (Join-Path $npmBin "opencode.cmd"),
        (Join-Path $tools "psmux.exe"),
        (Join-Path $tools "git-wt.exe")
    )
    foreach ($requiredFile in $requiredFiles) {
        if (-not (Test-Path -LiteralPath $requiredFile -PathType Leaf)) {
            throw "Required tool is missing after bootstrap: $requiredFile"
        }
    }

    Invoke-Checked $powerShell "--version"
    Invoke-Checked $git "--version"
    Invoke-Checked $node "--version"
    Invoke-Checked (Join-Path $cargoDirectory "rustc.exe") "--version"
    Invoke-Checked (Join-Path $cargoDirectory "cargo.exe") "--version"
    Invoke-Checked (Join-Path $npmBin "opencode.cmd") "--version"
    Invoke-Checked (Join-Path $tools "psmux.exe") "--version"
    Invoke-Checked (Join-Path $tools "git-wt.exe") "--version"

    $completion = @(
        "completed_utc=$([DateTime]::UtcNow.ToString('o'))",
        "powershell=$(& $powerShell --version)",
        "git=$(& $git --version)",
        "node=$(& $node --version)",
        "rust=$(& (Join-Path $cargoDirectory 'rustc.exe') --version)",
        "opencode=$(& (Join-Path $npmBin 'opencode.cmd') --version)",
        "psmux=$(& (Join-Path $tools 'psmux.exe') --version)",
        "worktrunk=$(& (Join-Path $tools 'git-wt.exe') --version)"
    )
    [System.IO.File]::WriteAllLines($completionPath, $completion, $utf8NoBom)
    Write-Log "Windows VM bootstrap completed"
    Write-Status "PASS"
}
catch {
    $message = $_.Exception.Message -replace "[`r`n]+", " "
    try { Write-Log "FAILED: $message" } catch { }
    try { Write-Status "FAIL $message" } catch { }
    exit 1
}
