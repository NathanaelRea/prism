[CmdletBinding()]
param(
    [switch] $SkipArchive
)

$ErrorActionPreference = "Stop"
if (-not $IsWindows -or $PSVersionTable.PSEdition -ne "Core") {
    throw "scripts/windows-check.ps1 requires native Windows and PowerShell 7"
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
Push-Location $repoRoot
try {
    $env:SQLX_OFFLINE = "true"
    $env:CARGO_INCREMENTAL = if ($env:CARGO_INCREMENTAL) { $env:CARGO_INCREMENTAL } else { "0" }

    Write-Host "==> cargo fmt --all -- --check"
    cargo fmt --all -- --check
    if ($LASTEXITCODE -ne 0) { throw "cargo fmt failed" }

    Write-Host "==> cargo check --locked --all-targets (SQLx offline)"
    cargo check --locked --all-targets
    if ($LASTEXITCODE -ne 0) { throw "cargo check failed" }

    Write-Host "==> cargo clippy --locked --all-targets -- -D warnings"
    cargo clippy --locked --all-targets -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw "cargo clippy failed" }

    Write-Host "==> cargo test --locked"
    cargo test --locked
    if ($LASTEXITCODE -ne 0) { throw "cargo test failed" }

    Write-Host "==> native Windows platform contracts"
    cargo test --locked platform_contract_ -- --test-threads=1
    if ($LASTEXITCODE -ne 0) { throw "platform contracts failed" }
    cargo test --locked windows_ -- --test-threads=1
    if ($LASTEXITCODE -ne 0) { throw "Windows native contracts failed" }

    if (-not $SkipArchive) {
        Write-Host "==> Windows release archive install smoke"
        cargo build --locked --release
        if ($LASTEXITCODE -ne 0) { throw "release build failed" }
        $archiveRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("prism-archive-" + [guid]::NewGuid().ToString("N"))
        $packageRoot = Join-Path $archiveRoot "prism-test-x86_64-pc-windows-msvc"
        $installRoot = "$archiveRoot-install"
        try {
            New-Item -ItemType Directory -Path $packageRoot -Force | Out-Null
            Copy-Item target/release/prism.exe (Join-Path $packageRoot "prism.exe")
            Copy-Item LICENSE.md (Join-Path $packageRoot "LICENSE.md")
            Copy-Item README.md (Join-Path $packageRoot "README.md")
            $archive = "${archiveRoot}.zip"
            Compress-Archive -Path $packageRoot -DestinationPath $archive
            Expand-Archive -Path $archive -DestinationPath $installRoot
            $installed = Get-ChildItem -LiteralPath $installRoot -Filter "prism.exe" -File -Recurse | Select-Object -First 1
            if ($null -eq $installed) { throw "archive omitted prism.exe" }
            & $installed.FullName --help | Out-Null
            if ($LASTEXITCODE -ne 0) { throw "extracted prism.exe failed --help" }
            $checksum = (Get-FileHash -Algorithm SHA256 $archive).Hash
            if ([string]::IsNullOrWhiteSpace($checksum)) { throw "archive checksum was empty" }
        }
        finally {
            Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $archiveRoot, $installRoot, "${archiveRoot}.zip"
        }
    }
}
finally {
    Pop-Location
}
