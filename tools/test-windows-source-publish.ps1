$ErrorActionPreference = 'Stop'
$installer = Join-Path $PSScriptRoot '../scripts/install.ps1'
$fixture = Join-Path ([IO.Path]::GetTempPath()) ('phonebox-publish-test-' + [guid]::NewGuid().ToString('N'))
$oldLocalAppData = $env:LOCALAPPDATA
$oldTargetDir = $env:CARGO_TARGET_DIR
$oldTemp = @{ TMP = $env:TMP; TEMP = $env:TEMP; TMPDIR = $env:TMPDIR }
$env:LOCALAPPDATA = $fixture
$app = Join-Path $fixture 'PhoneBox/app'
$installed = Join-Path $app 'PhoneBoxAgent.exe'
# install.ps1 stages the build in the temp folder: one of the fixture's, so a stage left behind is seen
$temp = Join-Path $fixture 'temp'
New-Item -ItemType Directory -Path $app, $temp -Force | Out-Null
$env:TMP = $temp; $env:TEMP = $temp; $env:TMPDIR = $temp
[IO.File]::WriteAllText($installed, 'running install')
$fakeState = [pscustomobject]@{ Stopped = $false; BuildExit = 0; WritesAgent = $false; Arguments = @() }

function Get-Process { 'fixture agent' }
function Stop-Process {
    param([Parameter(ValueFromPipeline)]$InputObject, [switch]$Force)
    process { $fakeState.Stopped = $true }
}
function Start-Sleep { }
# the agent is the Rust build: a stray dotnet call fails at once
function dotnet { throw 'dotnet is not used any more' }
# A fake `cargo build` that fails, or succeeds without leaving the agent; it writes only into the fixture's
# CARGO_TARGET_DIR, never into the real agent-rs\target.
function cargo {
    if (-not "$env:CARGO_TARGET_DIR".StartsWith($fixture)) { throw "cargo stub: CARGO_TARGET_DIR is '$env:CARGO_TARGET_DIR', not a folder of the test" }
    $fakeState.Arguments = @($args)
    $release = Join-Path $env:CARGO_TARGET_DIR 'release'
    New-Item -ItemType Directory -Path $release -Force | Out-Null
    [IO.File]::WriteAllText((Join-Path $release 'partial.txt'), 'incomplete build')
    if ($fakeState.WritesAgent) { [IO.File]::WriteAllText((Join-Path $release 'PhoneBoxAgent.exe'), 'incomplete build') }
    $global:LASTEXITCODE = $fakeState.BuildExit
}

try {
    $cases = @(
        @{ Exit = 7; WritesAgent = $true },   # cargo fails after writing part of the agent
        @{ Exit = 0; WritesAgent = $false }   # cargo succeeds, but no agent was built
    )
    $expected = @('build', '--release', '--locked', '--manifest-path', (Join-Path (Split-Path $PSScriptRoot) 'agent-rs/Cargo.toml'), '-p', 'phonebox-agent')
    $run = 0
    foreach ($case in $cases) {
        $run++
        $fakeState.BuildExit = $case.Exit
        $fakeState.WritesAgent = $case.WritesAgent
        $fakeState.Arguments = @()
        $env:CARGO_TARGET_DIR = Join-Path $fixture "target-$run"
        $message = ''
        try { & $installer } catch { $message = $_.Exception.Message }
        if ($message -notmatch 'build failed') { throw "Expected a build failure, got: $message" }
        if (($fakeState.Arguments -join ' ') -ne ($expected -join ' ')) { throw "Unexpected cargo arguments: $($fakeState.Arguments -join ' ')" }
        if ($fakeState.Stopped) { throw 'A failed or incomplete build stopped the running agent' }
        if ([IO.File]::ReadAllText($installed) -ne 'running install') { throw 'The existing binary changed' }
        if (Test-Path -LiteralPath (Join-Path $app 'partial.txt')) { throw 'The build wrote into the installed agent folder' }
        if (@(Get-ChildItem -LiteralPath $temp -Force -Filter 'phonebox-*').Count) { throw 'The staged build was left behind' }
    }
    'PASS: failed and incomplete builds leave the running installation alone'
} finally {
    $env:LOCALAPPDATA = $oldLocalAppData
    $env:CARGO_TARGET_DIR = $oldTargetDir
    foreach ($name in $oldTemp.Keys) { [Environment]::SetEnvironmentVariable($name, $oldTemp[$name]) }
    [IO.Directory]::Delete($fixture, $true)
}
