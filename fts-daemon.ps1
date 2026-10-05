# fts daemon control script (start / stop / restart / status).
# Uses a PID file + REST health check. All output in English to avoid
# codepage issues under Windows PowerShell 5.1.
#
# If config.json exists (and -Config is not explicitly ""), start uses
# `fts daemon --config config.json` and all address/dir options come from it.
#
# Usage:
#   .\fts-daemon.ps1 start  [-Config config.json] [-Rest ...] [-Listen ...] [-Out ...]
#   .\fts-daemon.ps1 stop
#   .\fts-daemon.ps1 restart
#   .\fts-daemon.ps1 status
param(
    [Parameter(Position = 0)]
    [ValidateSet("start", "stop", "restart", "status")]
    [string]$Action = "status",

    # Path to config file; used when it exists. Set to "" to force CLI options below.
    [string]$Config = "config.json",

    [string]$Rest = "127.0.0.1:5050",
    [string]$Listen = "0.0.0.0:4433",
    [string]$Out = ".\downloads",
    [string]$CertCache = ".\.fts",
    [int]$HealthTimeoutSec = 10
)

$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

$PidFile = Join-Path $CertCache "daemon.pid"
$Exe = Join-Path $PSScriptRoot "target\release\fts.exe"
# REST health-check port: from -Rest unless config file wins
$useConfig = ($Config -ne "" -and (Test-Path (Join-Path $PSScriptRoot $Config)))
    if ($useConfig) {
        try {
            $cfg = Get-Content (Join-Path $PSScriptRoot $Config) -Raw | ConvertFrom-Json
            if ($cfg.rest_addr) { $Rest = $cfg.rest_addr }
            if ($cfg.cert_cache -and -not (Test-Path $PidFile)) { $CertCache = $cfg.cert_cache }
            $PidFile = Join-Path $CertCache "daemon.pid"
        } catch { Write-Host "warning: failed to parse $Config ($($_.Exception.Message)); using CLI options" }
    }

function Get-DaemonPid {
    if (Test-Path $PidFile) {
        $daemonPid = Get-Content $PidFile -ErrorAction SilentlyContinue
        if ($daemonPid -and (Get-Process -Id $daemonPid -ErrorAction SilentlyContinue)) {
            return [int]$daemonPid
        }
    }
    return $null
}

function Test-RestAlive {
    try {
        $r = Invoke-WebRequest -UseBasicParsing -TimeoutSec 2 "http://$Rest/v1/capabilities"
        return ($r.StatusCode -eq 200)
    } catch { return $false }
}

function Start-Daemon {
    $existing = Get-DaemonPid
    if ($existing -or (Test-RestAlive)) {
        Write-Host "daemon already running (pid=$existing). Use 'restart' to reload."
        return
    }
    if (-not (Test-Path $Exe)) { throw "fts.exe not found at $Exe - run .\build.ps1 first" }

    New-Item -ItemType Directory -Force -Path $CertCache | Out-Null
    $log = Join-Path $CertCache "daemon.log"
    $errLog = Join-Path $CertCache "daemon.err.log"

    # config.json wins when present; otherwise explicit CLI options
    if ($useConfig) {
        $argList = @("daemon", "--config", $Config)
    } else {
        $argList = @("daemon", "--rest", $Rest, "--listen", $Listen, "--out", $Out, "--cert-cache", $CertCache)
    }
    $proc = Start-Process -FilePath $Exe `
        -ArgumentList $argList `
        -WindowStyle Hidden -RedirectStandardOutput $log -RedirectStandardError $errLog `
        -PassThru
    Set-Content -Path $PidFile -Value $proc.Id

    # wait for REST to come up
    $deadline = (Get-Date).AddSeconds($HealthTimeoutSec)
    while ((Get-Date) -lt $deadline) {
        if (Test-RestAlive) {
            Write-Host "daemon started: pid=$($proc.Id) rest=http://$Rest listen=$Listen out=$Out log=$log"
            return
        }
        if ($proc.HasExited) { throw "daemon exited immediately (code $($proc.ExitCode)). See $log" }
        Start-Sleep -Milliseconds 300
    }
    throw "daemon did not become healthy within ${HealthTimeoutSec}s. See $log"
}

function Stop-Daemon {
    $daemonPid = Get-DaemonPid
    if ($daemonPid) {
        Stop-Process -Id $daemonPid -Force -ErrorAction SilentlyContinue
        Write-Host "daemon stopped (pid=$daemonPid)"
    } elseif (Test-RestAlive) {
        # pid file lost but something answers on $Rest - find by name (only safe on single-instance hosts)
        Get-Process fts -ErrorAction SilentlyContinue | Stop-Process -Force
        Write-Host "daemon stopped (found by process name; pid file was stale/missing)"
    } else {
        Write-Host "daemon not running"
    }
    Remove-Item $PidFile -Force -ErrorAction SilentlyContinue
}

switch ($Action) {
    "start"   { Start-Daemon }
    "stop"    { Stop-Daemon }
    "restart" { Stop-Daemon; Start-Sleep -Milliseconds 500; Start-Daemon }
    "status"  {
        $daemonPid = Get-DaemonPid
        $alive = Test-RestAlive
        if ($daemonPid -and $alive) { Write-Host "running (pid=$daemonPid, rest=http://$Rest, healthy)" }
        elseif ($alive)             { Write-Host "running (rest=http://$Rest healthy, pid unknown)" }
        elseif ($daemonPid)         { Write-Host "degraded (pid=$daemonPid alive but REST not answering)" }
        else                        { Write-Host "stopped" }
    }
}
