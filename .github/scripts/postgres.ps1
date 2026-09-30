# Start the same password-authenticated fixture used on Linux/macOS.
$ErrorActionPreference = 'Stop'
$bin = Get-ChildItem 'C:\Program Files\PostgreSQL' -Directory |
    Sort-Object { [int]$_.Name } -Descending |
    ForEach-Object { Join-Path $_.FullName 'bin' } |
    Where-Object { Test-Path (Join-Path $_ 'initdb.exe') } |
    Select-Object -First 1
if (-not $bin) { throw 'PostgreSQL is required on the Windows CI runner' }
$data = Join-Path $env:RUNNER_TEMP 'tusk-pgdata'
$pw = Join-Path $env:RUNNER_TEMP 'tusk-pgpass'
[IO.File]::WriteAllText($pw, 'tusk')
function Invoke-Pg($tool, $arguments) {
    & (Join-Path $bin "$tool.exe") @arguments
    if ($LASTEXITCODE -ne 0) { throw "$tool failed with $LASTEXITCODE" }
}
Invoke-Pg 'initdb' @('-D', $data, '-U', 'tusk', "--pwfile=$pw", '--auth=scram-sha-256')
Invoke-Pg 'pg_ctl' @('-D', $data, '-o', '-p 55432', '-l', (Join-Path $env:RUNNER_TEMP 'tusk-pg.log'), '-w', 'start')
$env:PGPASSWORD = 'tusk'
Invoke-Pg 'createdb' @('-h', '127.0.0.1', '-p', '55432', '-U', 'tusk', 'tusk_dev')
Get-ChildItem 'seed/*.sql' | Sort-Object Name | ForEach-Object {
    Invoke-Pg 'psql' @('-q', '-v', 'ON_ERROR_STOP=1', '-h', '127.0.0.1', '-p', '55432', '-U', 'tusk', '-d', 'tusk_dev', '-f', $_.FullName)
}
