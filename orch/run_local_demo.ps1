#requires -Version 5.1
<#
  Single-machine 6-daemon simulation for the fts orchestration server.

  Starts three sources A1..A3 and three targets B1..B3 as separate daemons on
  distinct ports (each with its own cert cache, hence its own fingerprint),
  then runs three parallel transfers A1->B1 / A2->B2 / A3->B3 through the
  Python orchestrator (orch/cli.py) and prints the summary.

  This reproduces the multi-host A1->B1, A2->B2, A3->B3 scenario on one box
  when you have no spare hardware.

  Usage:
      powershell -ExecutionPolicy Bypass -File orch\run_local_demo.ps1
      powershell -ExecutionPolicy Bypass -File orch\run_local_demo.ps1 -SizeMB 256
      powershell -ExecutionPolicy Bypass -File orch\run_local_demo.ps1 -KeepRunning

  Notes: script text is English on purpose (PowerShell 5.1 reads BOM-less
  UTF-8 as ANSI and would mangle CJK literals).
#>
param(
  [int]$SizeMB = 64,
  [switch]$SkipBuild,
  [switch]$KeepRunning
)
$ErrorActionPreference = 'Stop'

$orch = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Split-Path -Parent $orch
$exe  = Join-Path $root 'target\release\fts.exe'
$demo = Join-Path $root '.demo'
$logs = Join-Path $demo 'logs'

# ---- 1. binary ------------------------------------------------------------
if (-not $SkipBuild) {
  if (-not (Test-Path $exe)) {
    Write-Host '[build] fts.exe missing, running cargo build --release ...'
    Push-Location $root; cargo build --release; Pop-Location
  }
}
if (-not (Test-Path $exe)) { throw "fts.exe not found at $exe (run build.ps1 first)" }

# ---- 2. layout ------------------------------------------------------------
Remove-Item -Recurse -Force $demo -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $logs | Out-Null

$specs = @(
  @{ name = 'A1'; grpc = 50051; listen = 4441 },
  @{ name = 'A2'; grpc = 50052; listen = 4442 },
  @{ name = 'A3'; grpc = 50053; listen = 4443 },
  @{ name = 'B1'; grpc = 50061; listen = 4451 },
  @{ name = 'B2'; grpc = 50062; listen = 4452 },
  @{ name = 'B3'; grpc = 50063; listen = 4453 }
)

function Wait-Port([int]$Port, [int]$TimeoutSec = 15) {
  $deadline = (Get-Date).AddSeconds($TimeoutSec)
  while ((Get-Date) -lt $deadline) {
    try {
      $c = New-Object System.Net.Sockets.TcpClient
      $c.Connect('127.0.0.1', $Port); $c.Close(); return $true
    } catch { Start-Sleep -Milliseconds 300 }
  }
  return $false
}

# ---- 3. start daemons -----------------------------------------------------
$procs = @()
foreach ($s in $specs) {
  $outdir = Join-Path $demo $s.name
  $cert   = Join-Path $demo ('cert-' + $s.name)
  New-Item -ItemType Directory -Force -Path $outdir | Out-Null
  $args = @(
    'daemon',
    '--rest',   "127.0.0.1:$($s.grpc + 1000)",
    '--grpc',   "127.0.0.1:$($s.grpc)",
    '--listen', "127.0.0.1:$($s.listen)",
    '--out',    $outdir,
    '--cert-cache', $cert,
    '--max-sessions', '3'
  )
  $p = Start-Process -FilePath $exe -ArgumentList $args -WindowStyle Hidden -PassThru `
        -RedirectStandardOutput (Join-Path $logs "$($s.name).out.log") `
        -RedirectStandardError  (Join-Path $logs "$($s.name).err.log")
  $procs += $p
  $s.pid = $p.Id
  Write-Host ("[start] {0} pid={1} grpc=127.0.0.1:{2} data=127.0.0.1:{3}" -f $s.name, $p.Id, $s.grpc, $s.listen)
}

try {
  foreach ($s in $specs) {
    if (-not (Wait-Port $s.grpc 15)) { throw "daemon $($s.name) gRPC port $($s.grpc) not listening" }
  }
  Write-Host '[ok] all 6 daemons up'

  # ---- 4. test payload ----------------------------------------------------
  $srcFile = Join-Path $demo 'payload.bin'
  $fs = [System.IO.File]::Create($srcFile)
  $fs.SetLength([int64]$SizeMB * 1MB)
  $fs.Close()
  Write-Host "[gen] $srcFile ($SizeMB MiB)"

  foreach ($i in 1..3) { New-Item -ItemType Directory -Force -Path (Join-Path $demo "recv\B$i") | Out-Null }

  # ---- 5. orchestrator config --------------------------------------------
  $endpoints = foreach ($s in $specs) { @{ name = $s.name; grpc_addr = "127.0.0.1:$($s.grpc)" } }
  $tasks = foreach ($i in 1..3) {
    @{ name = "A$i->B$i"; src = "A$i"; src_path = $srcFile; dst = "B$i"; dst_path = (Join-Path $demo "recv\B$i") }
  }
  $cfg = @{
    timeout_sec = 3
    defaults    = @{ transport = 'auto'; streams = 0; chunk_size = 0 }
    endpoints   = @($endpoints)
    tasks       = @($tasks)
  }
  $cfgPath = Join-Path $orch 'config.local.json'
  $cfg | ConvertTo-Json -Depth 8 | Out-File -FilePath $cfgPath -Encoding utf8
  Write-Host "[cfg] $cfgPath"

  # ---- 6. run -------------------------------------------------------------
  Push-Location $orch
  Write-Host "`n=== health check ==="
  python cli.py --config config.local.json --check
  Write-Host "`n=== parallel A1->B1 / A2->B2 / A3->B3 ==="
  python cli.py --config config.local.json --run-config-tasks
  $rc = $LASTEXITCODE
  Pop-Location

  # ---- 7. verify landed files --------------------------------------------
  Write-Host "`n=== landed files ==="
  $ok = $true
  foreach ($i in 1..3) {
    $dst = Join-Path $demo "recv\B$i\payload.bin"
    if (Test-Path $dst) {
      $len = (Get-Item $dst).Length
      $good = ($len -eq ([int64]$SizeMB * 1MB))
      if (-not $good) { $ok = $false }
      Write-Host ("[{0}] {1}  {2} bytes" -f ($(if ($good) {'OK'} else {'BAD'})), $dst, $len)
    } else {
      $ok = $false
      Write-Host "[MISS] $dst"
    }
  }

  if ($rc -ne 0 -or -not $ok) { Write-Host "`nDEMO FAILED (rc=$rc)"; exit 1 }
  Write-Host "`nDEMO PASSED"
}
finally {
  if (-not $KeepRunning) {
    foreach ($p in $procs) {
      if ($p -and -not $p.HasExited) { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue }
    }
    Write-Host '[cleanup] daemons stopped'
  } else {
    Write-Host '[cleanup] -KeepRunning set; daemons left running'
    foreach ($s in $specs) { Write-Host ("  {0}: pid={1} grpc=127.0.0.1:{2}" -f $s.name, $s.pid, $s.grpc) }
  }
}
