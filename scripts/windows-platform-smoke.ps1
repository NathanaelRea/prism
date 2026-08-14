[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
if (-not $IsWindows -or $PSVersionTable.PSEdition -ne "Core") {
    throw "scripts/windows-platform-smoke.ps1 requires native Windows and PowerShell 7"
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path

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

function Resolve-NativeCommand {
    param([Parameter(Mandatory)] [string] $Name)
    $command = Get-Command $Name -ErrorAction Stop | Select-Object -First 1
    return $command.Source
}

function ConvertTo-TomlString {
    param([Parameter(Mandatory)] [string] $Value)
    return $Value.Replace("\", "\\").Replace('"', '\"')
}

function Read-OutputValue {
    param([string[]] $Lines, [string] $Name)
    $prefix = "$Name = "
    $line = $Lines | Where-Object { $_.StartsWith($prefix) } | Select-Object -First 1
    if ($null -eq $line) { throw "Prism output omitted $Name" }
    return $line.Substring($prefix.Length)
}

Push-Location $repoRoot
$root = Join-Path ([System.IO.Path]::GetTempPath()) ("prism-windows-smoke-" + [guid]::NewGuid().ToString("N"))
$runtimePid = $null
$session = $null
try {
    $git = Resolve-NativeCommand "git.exe"
    $psmux = if ($env:PRISM_TEST_PSMUX) { (Resolve-Path $env:PRISM_TEST_PSMUX).Path } else { Resolve-NativeCommand "psmux.exe" }
    $worktrunk = if ($env:PRISM_TEST_WORKTRUNK) { (Resolve-Path $env:PRISM_TEST_WORKTRUNK).Path } else { Resolve-NativeCommand "git-wt.exe" }
    $opencode = if ($env:PRISM_TEST_OPENCODE) { (Resolve-Path $env:PRISM_TEST_OPENCODE).Path } else { Resolve-NativeCommand "opencode.cmd" }
    $env:PRISM_TEST_OPENCODE = $opencode

    Write-Host "==> psmux command and ConPTY contracts"
    $env:PRISM_WINDOWS_SPIKE_PSMUX = $psmux
    & (Join-Path $PSScriptRoot "windows-phase0-spikes.ps1") -Spike psmux
    if ($LASTEXITCODE -ne 0) { throw "psmux contract failed" }

    New-Item -ItemType Directory -Path $root | Out-Null
    $env:WORKTRUNK_CONFIG_PATH = Join-Path $root "worktrunk.toml"
    $env:WORKTRUNK_WORKTREE_PATH = Join-Path $root 'worktrees/{{ branch | sanitize }}'
    $env:PRISM_TEST_WORKTRUNK = $worktrunk
    Write-Host "==> real Git and Worktrunk compatibility smoke"
    Invoke-Checked cargo "test" "repository::worktrunk::tests::real_worktrunk_create_observe_remove_smoke" "--" "--ignored" "--exact" "--nocapture"

    Write-Host "==> real no-model OpenCode session API smoke"
    $env:APPDATA = Join-Path $root "api app data"
    $env:LOCALAPPDATA = Join-Path $root "api local app data"
    New-Item -ItemType Directory -Force -Path $env:APPDATA, $env:LOCALAPPDATA | Out-Null
    Invoke-Checked cargo "test" "agent_runtime::opencode::tests::real_opencode_server_round_trips_prism_session_api" "--" "--ignored" "--exact" "--nocapture" "--test-threads=1"

    Write-Host "==> real no-model Prism, OpenCode, and psmux stack"
    $stackTarget = Join-Path $root "prism stack target"
    Invoke-Checked cargo "build" "--locked" "--target-dir" $stackTarget
    $prism = Join-Path $stackTarget "debug/prism.exe"
    $repo = Join-Path $root "repo with spaces and 雪"
    $worktree = Join-Path $root "worktree feature 雪"
    New-Item -ItemType Directory -Path $repo | Out-Null
    Invoke-Checked $git "-C" $repo "init" "--initial-branch=main"
    Invoke-Checked $git "-C" $repo "config" "user.name" "Prism Windows Smoke"
    Invoke-Checked $git "-C" $repo "config" "user.email" "prism@example.invalid"
    Set-Content -LiteralPath (Join-Path $repo "README.md") -Value "Prism Windows smoke" -Encoding utf8NoBOM
    Invoke-Checked $git "-C" $repo "add" "README.md"
    Invoke-Checked $git "-C" $repo "commit" "-m" "initial"
    Invoke-Checked $git "-C" $repo "worktree" "add" "-b" "feature/windows-smoke" $worktree

    $appData = Join-Path $root "app data"
    $configDir = Join-Path $appData "Prism"
    $opencodeHome = Join-Path $root "opencode home"
    $opencodeConfig = Join-Path $root "opencode config"
    $opencodeData = Join-Path $root "opencode data"
    New-Item -ItemType Directory -Force -Path $configDir, $opencodeHome, $opencodeConfig, $opencodeData | Out-Null
    $env:APPDATA = $appData
    $env:LOCALAPPDATA = Join-Path $root "local app data"
    $env:PRISM_RUNTIME_DIR = Join-Path $root "runtime"
    $env:HOME = $opencodeHome
    $env:OPENCODE_CONFIG_DIR = $opencodeConfig
    $env:XDG_DATA_HOME = $opencodeData
    $env:OPENCODE_DISABLE_AUTOUPDATE = "true"
    $env:OPENCODE_DISABLE_DEFAULT_PLUGINS = "true"
    $env:OPENCODE_DISABLE_LSP_DOWNLOAD = "true"
    $env:OPENCODE_DISABLE_MODELS_FETCH = "true"

    $config = @"
default_harness = "opencode"
default_base = "main"
opencode_port_base = 43000
opencode_port_span = 1000

[harnesses.opencode]
adapter = "opencode"
program = "$(ConvertTo-TomlString $opencode)"

[tools]
tmux = "$(ConvertTo-TomlString $psmux)"
git = "$(ConvertTo-TomlString $git)"
"git-wt.exe" = "$(ConvertTo-TomlString $worktrunk)"
"@
    Set-Content -LiteralPath (Join-Path $configDir "config.toml") -Value $config -Encoding utf8NoBOM

    $doctorOutput = @(& $prism doctor --repo $repo 2>&1 | ForEach-Object { $_.ToString() })
    if ($LASTEXITCODE -ne 0) { throw "Prism doctor failed with native tools:`n$($doctorOutput -join "`n")" }
    $doctorText = $doctorOutput -join "`n"
    foreach ($pattern in @(
        "selected harness: opencode",
        "resolved program: .*opencode\.cmd",
        "(?m)^ok\s+git\s+.*git\.exe",
        "(?m)^ok\s+tmux\s+.*psmux\.exe",
        "(?m)^ok\s+git-wt\.exe\s+.*git-wt\.exe",
        "worktrunk observation: fresh"
    )) {
        if ($doctorText -notmatch $pattern) {
            throw "Prism doctor omitted native tool evidence matching: $pattern`n$doctorText"
        }
    }

    $first = @(& $prism --repo $repo agent ensure --branch "feature/windows-smoke" 2>&1 | ForEach-Object { $_.ToString() })
    if ($LASTEXITCODE -ne 0) { throw "first agent ensure failed:`n$($first -join "`n")" }
    $session = Read-OutputValue $first "tmux_session"
    $sessionId = Read-OutputValue $first "session_id"
    $runtimePid = Read-OutputValue $first "runtime_process_id"
    if ([string]::IsNullOrWhiteSpace($sessionId) -or [string]::IsNullOrWhiteSpace($runtimePid)) {
        throw "agent ensure did not create an OpenCode runtime and session"
    }

    $second = @(& $prism --repo $repo agent ensure --branch "feature/windows-smoke" 2>&1 | ForEach-Object { $_.ToString() })
    if ($LASTEXITCODE -ne 0) { throw "second agent ensure failed:`n$($second -join "`n")" }
    if ((Read-OutputValue $second "tmux_session") -ne $session -or (Read-OutputValue $second "session_id") -ne $sessionId -or (Read-OutputValue $second "runtime_process_id") -ne $runtimePid) {
        throw "second ensure did not reuse the psmux/OpenCode session"
    }

    Invoke-Checked $psmux "kill-session" "-t" $session
    Invoke-Checked $psmux "new-session" "-d" "-s" $session "-n" "stale" "-c" $repo "pwsh.exe -NoLogo -NoProfile"
    $third = @(& $prism --repo $repo agent ensure --branch "feature/windows-smoke" 2>&1 | ForEach-Object { $_.ToString() })
    if ($LASTEXITCODE -ne 0) { throw "replacement agent ensure failed:`n$($third -join "`n")" }
    if ((Read-OutputValue $third "tmux_session") -ne $session -or (Read-OutputValue $third "session_id") -ne $sessionId -or (Read-OutputValue $third "runtime_process_id") -ne $runtimePid) {
        throw "replacement ensure did not retain the durable psmux/OpenCode identity"
    }
    $replacementWindows = @(& $psmux list-windows -t $session -F "#{window_name}")
    if ($LASTEXITCODE -ne 0 -or $replacementWindows -contains "stale" -or $replacementWindows -notcontains "agent") {
        throw "Prism did not replace the stale psmux session with a configured agent session"
    }

    $prompt = Join-Path $root "prompt with spaces and 雪.txt"
    $marker = "PRISM_WINDOWS_PROMPT_雪_" + [guid]::NewGuid().ToString("N")
    Set-Content -LiteralPath $prompt -Value $marker -Encoding utf8NoBOM -NoNewline
    Invoke-Checked $psmux "load-buffer" "-b" "prism-windows-smoke" $prompt
    Invoke-Checked $psmux "paste-buffer" "-d" "-b" "prism-windows-smoke" "-t" "${session}:1"
    $deadline = [DateTime]::UtcNow.AddSeconds(10)
    do {
        $capture = @(& $psmux capture-pane -p -t "${session}:1" 2>$null) -join "`n"
        if ($capture.Contains($marker)) { break }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $deadline)
    if (-not $capture.Contains($marker)) { throw "psmux capture did not contain the pasted Unicode prompt" }

    Invoke-Checked $psmux "send-keys" "-t" "${session}:1" "Enter"
    $sessionEndpoint = Read-OutputValue $first "session_endpoint"
    $sessionPath = [Uri]::EscapeDataString($sessionId)
    $directoryQuery = [Uri]::EscapeDataString($worktree)
    $messagesUrl = "$sessionEndpoint/session/$sessionPath/message?directory=$directoryQuery"
    $deadline = [DateTime]::UtcNow.AddSeconds(15)
    $persisted = $false
    do {
        try {
            $messages = Invoke-RestMethod -Method Get -Uri $messagesUrl -TimeoutSec 2
            $messagesJson = $messages | ConvertTo-Json -Depth 20 -Compress
            $persisted = $messagesJson.Contains($marker)
        }
        catch {
            $persisted = $false
        }
        if (-not $persisted) { Start-Sleep -Milliseconds 100 }
    } while (-not $persisted -and [DateTime]::UtcNow -lt $deadline)
    if (-not $persisted) { throw "OpenCode session API did not persist the psmux-submitted prompt" }

    Invoke-Checked $psmux "kill-session" "-t" $session
    & $psmux has-session -t $session 2>$null
    if ($LASTEXITCODE -eq 0) { throw "psmux session survived cleanup" }
    $session = $null
    Write-Host "Windows full-stack smoke PASS"
}
finally {
    if ($session) { & $psmux kill-session -t $session 2>$null | Out-Null }
    if ($runtimePid -and $runtimePid -match '^\d+$') {
        Stop-Process -Id ([int]$runtimePid) -Force -ErrorAction SilentlyContinue
    }
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $root
    Pop-Location
}
