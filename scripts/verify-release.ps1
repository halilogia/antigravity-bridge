# Verify only local release endpoints with a disposable configuration and synthetic key.
[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
$project = Split-Path -Parent $PSScriptRoot
$binary = Join-Path $project 'target\release\antigravity-bridge.exe'
if (-not (Test-Path -LiteralPath $binary)) { throw 'Build the release binary first.' }
$runDir = Join-Path $project ('target\release-smoke-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $runDir | Out-Null
$configPath = Join-Path $runDir 'config.json'
& $binary --config $configPath init | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'Release init failed.' }
$config = Get-Content -LiteralPath $configPath -Raw | ConvertFrom-Json
$probe = New-Object System.Net.Sockets.TcpListener([Net.IPAddress]::Loopback, 0)
$probe.Start()
$port = $probe.LocalEndpoint.Port
$probe.Stop()
$config.listen = '127.0.0.1:' + $port
# UTF-8 without BOM: serde_json deliberately rejects a BOM.
[IO.File]::WriteAllText($configPath, ($config | ConvertTo-Json -Depth 30), (New-Object Text.UTF8Encoding($false)))
$base = 'http://127.0.0.1:' + $port
$process = Start-Process -FilePath $binary -ArgumentList @('--config', ('"{0}"' -f $configPath), 'serve') -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $runDir 'stdout.log') -RedirectStandardError (Join-Path $runDir 'stderr.log')
try {
    $ready = $false
    for ($i = 0; $i -lt 50; $i++) {
        try { $health = Invoke-RestMethod -Uri ($base + '/health') -TimeoutSec 1; $ready = $true; break } catch { Start-Sleep -Milliseconds 200 }
    }
    if (-not $ready -or $health.status -ne 'ok') { throw 'Release health check failed.' }
    $ui = Invoke-WebRequest -UseBasicParsing -Uri ($base + '/ui')
    if (-not $ui.Content.Contains('Antigravity Model Bridge')) { throw 'UI asset missing.' }
    $headers = @{ 'x-admin-token' = $config.admin_token }
    $syntheticKey = 'release-smoke-synthetic-key-not-a-real-credential'
    $setup = @{plan='go';api_key=$syntheticKey} | ConvertTo-Json
    Invoke-RestMethod -Uri ($base + '/admin/opencode/setup') -Method Post -ContentType 'application/json' -Headers $headers -Body $setup | Out-Null
    $add = @{provider='opencode-go';ids=@('smoke-model')} | ConvertTo-Json
    Invoke-RestMethod -Uri ($base + '/admin/opencode/models') -Method Post -ContentType 'application/json' -Headers $headers -Body $add | Out-Null
    $view = Invoke-RestMethod -Uri ($base + '/admin/config') -Headers $headers
    if ($view.providers.'opencode-go'.keySource -ne 'secret' -or $view.models.Count -ne 1) { throw 'Encrypted-key/model CRUD check failed.' }
    $raw = Get-Content -LiteralPath (Join-Path $runDir 'secrets.json') -Raw
    if ($raw.Contains($syntheticKey)) { throw 'Plaintext key detected.' }
    $heartbeat = Get-Content -LiteralPath (Join-Path $runDir 'bridge.json') -Raw | ConvertFrom-Json
    if ($heartbeat.pid -ne $process.Id -or $heartbeat.url -ne $base) { throw 'Heartbeat mismatch.' }
    $unauthorized = 0
    try { Invoke-WebRequest -UseBasicParsing -Uri ($base + '/admin/config') | Out-Null } catch { $unauthorized = [int]$_.Exception.Response.StatusCode }
    if ($unauthorized -ne 401) { throw 'Admin authorization check failed.' }
    $crossOrigin = 0
    try { Invoke-WebRequest -UseBasicParsing -Uri ($base + '/health') -Headers @{Origin='https://example.invalid'} | Out-Null } catch { $crossOrigin = [int]$_.Exception.Response.StatusCode }
    if ($crossOrigin -ne 403) { throw 'Origin guard check failed.' }
    [pscustomobject]@{status='passed'; health=$health.status; configuredModels=$view.models.Count; keySource='secret'; heartbeat='matched'; unauthorizedStatus=$unauthorized; crossOriginStatus=$crossOrigin; binarySha256=(Get-FileHash -LiteralPath $binary -Algorithm SHA256).Hash.ToLowerInvariant()} | ConvertTo-Json
} finally {
    if (-not $process.HasExited) { Stop-Process -Id $process.Id }
}
