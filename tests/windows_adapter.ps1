# Windows check of the adapter protocol through the real pgbx.exe (the Unix twin is cli/tests/adapter_cli.rs):
# an adapter is started and stopped per command, one that ignores stop is killed with its whole Job Object (its own
# child too), a pgbx killed mid-run leaves no adapter behind, and the password never reaches output or disk.
# No Postgres needed: the url points at a closed port, so the command fails AFTER the adapter answered.
# Run:  powershell -NoProfile -ExecutionPolicy Bypass -File tests\windows_adapter.ps1 -Pgbx C:\path\pgbx.exe
param([Parameter(Mandatory = $true)][string]$Pgbx)
$ErrorActionPreference = 'Stop'
$pass = 0; $fail = 0
function Check($what, $got, $want) {
    if ("$got" -eq "$want") { Write-Host "  PASS $what"; $script:pass++ }
    else { Write-Host "  FAIL $what (got '$got', want '$want')"; $script:fail++ }
}
function Gone($procId) { -not (Get-Process -Id $procId -ErrorAction SilentlyContinue) }
function WaitFor($path, $secs) { $t = 0; while (-not (Test-Path $path) -and $t -lt ($secs * 10)) { Start-Sleep -Milliseconds 100; $t++ }; Test-Path $path }

$W = Join-Path ([IO.Path]::GetTempPath()) ("pgbx-win-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force (Join-Path $W 'cfg') | Out-Null
$pw = 'Win-Secret-Pw-456'
$env:PGBX_TEST_PW = $pw
$env:PGBX_CONFIG_DIR = Join-Path $W 'cfg'

# the fake adapter: MODE (normal | stubborn | slow) and DIR come from the profile config in the start line
$fake = @'
$line = [Console]::In.ReadLine()
if (-not $line) { exit 0 }
$msg = $line | ConvertFrom-Json
$dir = $msg.config.dir; $mode = $msg.config.mode
"$PID" | Set-Content (Join-Path $dir "$mode.pid")
[Console]::Error.WriteLine("fake: connecting with password $($msg.config.password)")
if ($mode -eq 'slow') { Start-Sleep -Seconds 20 }
if ($mode -eq 'stubborn') {
    $c = Start-Process -FilePath ping -ArgumentList '-t','127.0.0.1' -WindowStyle Hidden -PassThru
    "$($c.Id)" | Set-Content (Join-Path $dir 'grandchild.pid')
}
$url = "postgres://app:$($msg.config.password)@127.0.0.1:1/shop"
[Console]::Out.WriteLine((@{ url = $url; state = 'ready'; name = $msg.name } | ConvertTo-Json -Compress))
[Console]::Out.Flush()
while ($true) {
    $l = [Console]::In.ReadLine()
    if ($null -eq $l) { 'eof' | Set-Content (Join-Path $dir "$mode.eof"); exit 0 }
    if ($l -match 'stop') {
        if ($mode -eq 'stubborn') { continue }          # ignores stop: pgbx must kill the whole job
        'stop' | Set-Content (Join-Path $dir "$mode.stopped"); exit 0
    }
}
'@
$fakePath = Join-Path $W 'fake.ps1'
Set-Content -Path $fakePath -Value $fake
$ps = (Get-Command powershell).Source
$cfg = @"
adapters:
  fake: ["$($ps -replace '\\','\\')", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", "$($fakePath -replace '\\','\\')"]
profiles:
  normal:   { adapter: fake, mode: normal,   dir: "$($W -replace '\\','\\')", password: `$PGBX_TEST_PW }
  stubborn: { adapter: fake, mode: stubborn, dir: "$($W -replace '\\','\\')", password: `$PGBX_TEST_PW }
  slow:     { adapter: fake, mode: slow,     dir: "$($W -replace '\\','\\')", password: `$PGBX_TEST_PW, ready_timeout: 60s }
"@
Set-Content -Path (Join-Path $W 'cfg\config.yaml') -Value $cfg

Write-Host "## pgbx: $(& $Pgbx --version)"

Write-Host "## a. a one-off command starts its adapter and stops it"
$out = & $Pgbx query "SELECT 1" --profile normal --json 2>&1 | Out-String
Check "adapter started" (Test-Path (Join-Path $W 'normal.pid')) $true
Check "adapter got stop" (WaitFor (Join-Path $W 'normal.stopped') 10) $true
$apid = [int](Get-Content (Join-Path $W 'normal.pid'))
Start-Sleep -Milliseconds 500
Check "adapter process gone" (Gone $apid) $true
Check "password not in pgbx output" ($out.Contains($pw)) $false

Write-Host "## b. an adapter that ignores stop is killed with its whole job (its own child too)"
$t0 = Get-Date
$out = & $Pgbx query "SELECT 1" --profile stubborn --json 2>&1 | Out-String
$took = ((Get-Date) - $t0).TotalSeconds
$apid = [int](Get-Content (Join-Path $W 'stubborn.pid'))
$gpid = [int](Get-Content (Join-Path $W 'grandchild.pid'))
Start-Sleep -Milliseconds 500
Check "stubborn adapter killed" (Gone $apid) $true
Check "its child (ping -t) killed with the job" (Gone $gpid) $true
Check "pgbx waited no more than grace + margin (<15 s)" ($took -lt 15) $true
Check "password not in pgbx output" ($out.Contains($pw)) $false

Write-Host "## c. pgbx killed while its adapter is starting: nothing is left behind"
$p = Start-Process -FilePath $Pgbx -ArgumentList 'query','SELECT 1','--profile','slow','--json' -WindowStyle Hidden -PassThru
WaitFor (Join-Path $W 'slow.pid') 15 | Out-Null
$apid = [int](Get-Content (Join-Path $W 'slow.pid'))
Stop-Process -Id $p.Id -Force
$t = 0; while (-not (Gone $apid) -and $t -lt 100) { Start-Sleep -Milliseconds 100; $t++ }
Check "adapter gone after pgbx was killed (job closed or EOF)" (Gone $apid) $true

Write-Host "## d. no secret on disk"
$hits = Get-ChildItem -Recurse -File $env:PGBX_CONFIG_DIR | Select-String -SimpleMatch $pw -List
Check "password not in the config dir" ($null -eq $hits) $true

Remove-Item -Recurse -Force $W -ErrorAction SilentlyContinue
Write-Host "== windows_adapter: $pass passed, $fail failed"
if ($fail -ne 0) { exit 1 }
