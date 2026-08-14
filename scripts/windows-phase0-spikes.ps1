[CmdletBinding()]
param(
    [ValidateSet("all", "acl", "process", "worker-ipc", "recorder", "persistence", "psmux")]
    [string] $Spike = "all",
    [string] $Executable,
    [switch] $CheckOnly
)

$ErrorActionPreference = "Stop"
if ($PSVersionTable.PSEdition -ne "Core" -or $PSVersionTable.PSVersion.Major -lt 7) {
    throw "Prism's Windows feasibility spikes require PowerShell 7 or newer"
}
if (-not $IsWindows) {
    throw "Prism's Windows feasibility spikes must run on native Windows"
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$manifest = Join-Path $repoRoot "spikes/windows/Cargo.toml"
$psmuxVersion = "3.3.7"
$psmuxSha256 = "60ff7b236f64184921cef3c1ff2611aa5a36fcc7ed8e2a58e968b8ded57f6028"
$downloadRoot = $null

function Invoke-CheckedNative {
    param(
        [Parameter(Mandatory)] [string] $Program,
        [Parameter(ValueFromRemainingArguments)] [string[]] $NativeArgs
    )

    & $Program @NativeArgs
    if ($LASTEXITCODE -ne 0) {
        throw "$Program exited with status $LASTEXITCODE"
    }
}

try {
    $runPsmux = -not $CheckOnly -and $Spike -in @("all", "psmux")
    if ($runPsmux) {
        $psmux = $env:PRISM_WINDOWS_SPIKE_PSMUX
        if ([string]::IsNullOrWhiteSpace($psmux)) {
            $downloadRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("prism-psmux-spike-" + [guid]::NewGuid().ToString("N"))
            New-Item -ItemType Directory -Path $downloadRoot | Out-Null
            $archive = Join-Path $downloadRoot "psmux.zip"
            $url = "https://github.com/psmux/psmux/releases/download/v$psmuxVersion/psmux-v$psmuxVersion-windows-x64.zip"
            Write-Host "==> Download pinned psmux $psmuxVersion"
            Invoke-WebRequest -Uri $url -OutFile $archive
            $actualHash = (Get-FileHash -Algorithm SHA256 -Path $archive).Hash.ToLowerInvariant()
            if ($actualHash -ne $psmuxSha256) {
                throw "psmux archive checksum mismatch: expected $psmuxSha256, got $actualHash"
            }
            Expand-Archive -Path $archive -DestinationPath (Join-Path $downloadRoot "psmux")
            $psmux = Get-ChildItem -Path (Join-Path $downloadRoot "psmux") -Filter psmux.exe -File -Recurse |
                Select-Object -First 1 -ExpandProperty FullName
            if ([string]::IsNullOrWhiteSpace($psmux)) {
                throw "the pinned psmux archive did not contain psmux.exe"
            }
        }

        $env:PRISM_WINDOWS_SPIKE_PSMUX = (Resolve-Path $psmux).Path
        Write-Host "==> $env:PRISM_WINDOWS_SPIKE_PSMUX --version"
        Invoke-CheckedNative $env:PRISM_WINDOWS_SPIKE_PSMUX "--version"
    }

    Push-Location $repoRoot
    try {
        if (-not [string]::IsNullOrWhiteSpace($Executable)) {
            $resolvedExecutable = (Resolve-Path $Executable).Path
            Write-Host "==> $resolvedExecutable --spike $Spike"
            Invoke-CheckedNative $resolvedExecutable "--spike" $Spike
        }
        else {
            Write-Host "==> cargo fmt --check (Windows spike crate)"
            Invoke-CheckedNative cargo "fmt" "--manifest-path" $manifest "--" "--check"
            Write-Host "==> cargo clippy (Windows spike crate)"
            Invoke-CheckedNative cargo "clippy" "--locked" "--manifest-path" $manifest "--" "-D" "warnings"
            if ($CheckOnly) {
                Write-Host "==> cargo build (Windows spike crate)"
                Invoke-CheckedNative cargo "build" "--locked" "--manifest-path" $manifest
            }
            else {
                Write-Host "==> cargo run (Windows spike: $Spike)"
                Invoke-CheckedNative cargo "run" "--locked" "--manifest-path" $manifest "--" "--spike" $Spike
            }
        }
    }
    finally {
        Pop-Location
    }
}
finally {
    if ($null -ne $downloadRoot -and (Test-Path $downloadRoot)) {
        Remove-Item -Path $downloadRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}
