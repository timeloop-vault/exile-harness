# Spike demo: prove headless Path of Building on Windows for one engine.
#
#   .\demo.ps1 -Game poe1 -BuildXml ..\..\vendor\pob\poe1\spec\TestBuilds\3.13\OccVortex.xml
#   .\demo.ps1 -Game poe2
#
# Two passes, each a fresh engine process, driven purely over the
# JSON-lines protocol like a real host would:
#   pass 1: version -> load a build (fixture XML, or a fresh build when
#           no fixture exists) -> export it as a build CODE
#   pass 2: version -> import that CODE -> read core stats
param(
    [ValidateSet("poe1", "poe2")]
    [string]$Game = "poe1",
    [string]$BuildXml,
    [string]$LuaJit
)

$ErrorActionPreference = "Stop"
$OutputEncoding = New-Object System.Text.UTF8Encoding($false)

if (-not $LuaJit) {
    $found = Get-Command luajit -ErrorAction SilentlyContinue
    if ($found) { $LuaJit = $found.Source }
    else { $LuaJit = Join-Path $env:LOCALAPPDATA "Programs\LuaJIT\bin\luajit.exe" }
}
if (-not (Test-Path $LuaJit)) { throw "luajit not found; pass -LuaJit <path>" }

$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$engineSrc = Join-Path $repoRoot "vendor\pob\$Game\src"
if (-not (Test-Path (Join-Path $engineSrc "HeadlessWrapper.lua"))) {
    throw "no engine checkout at $engineSrc (see README.md for clone instructions)"
}
$adapter = Join-Path $PSScriptRoot "adapter.lua"

function Invoke-Adapter([string[]]$Requests) {
    Push-Location $engineSrc
    try { $stdout = $Requests | & $LuaJit $adapter }
    finally { Pop-Location }
    $stdout | ForEach-Object { $_ | ConvertFrom-Json } | Where-Object { $null -ne $_.id }
}

function Get-Reply($Replies, [int]$Id) {
    $reply = $Replies | Where-Object { $_.id -eq $Id }
    if (-not $reply.ok) { throw "request $Id failed: $($reply.error)" }
    $reply.result
}

# Pass 1: obtain a build code from the engine itself.
$load = if ($BuildXml) {
    # ReadAllText, not Get-Content -Raw: PowerShell 5.1 decorates the
    # latter's string with note properties, which ConvertTo-Json then
    # serializes as an object instead of a string.
    $xml = [IO.File]::ReadAllText((Resolve-Path $BuildXml))
    @{ id = 2; cmd = "loadXML"; xml = $xml; name = "demo" } | ConvertTo-Json -Compress
} else {
    '{"id":2,"cmd":"new"}'
}
$replies = Invoke-Adapter @(
    '{"id":1,"cmd":"version"}'
    $load
    '{"id":3,"cmd":"makeCode"}'
    '{"id":4,"cmd":"quit"}'
)
$version = (Get-Reply $replies 1).engine
$null = Get-Reply $replies 2   # a failed load must fail the demo
$code = (Get-Reply $replies 3).code
Write-Host "engine  : $Game (PoB $version)"
Write-Host "code    : $($code.Substring(0, [Math]::Min(60, $code.Length)))... ($($code.Length) chars)"

# Pass 2: fresh process, import the code, read stats.
$replies = Invoke-Adapter @(
    '{"id":1,"cmd":"version"}'
    (@{ id = 2; cmd = "loadCode"; code = $code; name = "demo-code" } | ConvertTo-Json -Compress)
    '{"id":3,"cmd":"stats"}'
    '{"id":4,"cmd":"quit"}'
)
$null = Get-Reply $replies 2   # a failed import must fail the demo
$stats = Get-Reply $replies 3
Write-Host "stats   :"
$stats.PSObject.Properties | Sort-Object Name | ForEach-Object {
    Write-Host ("  {0,-16} {1}" -f $_.Name, $_.Value)
}
