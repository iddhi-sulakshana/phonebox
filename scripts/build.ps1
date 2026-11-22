<#
.SYNOPSIS
  Builds release artifacts for this computer into dist\:
    PhoneBox-<version>.apk               Android panel (release-signed if a key is configured)
    PhoneBoxAgent-<version>-<rid>.zip    agent for this computer's system and CPU (win-x64, osx-arm64, ...)
  The version is agent-rs\Cargo.toml's. cargo builds only for the computer it runs on: the release builds the agent
  for every system on GitHub's runners (.github\workflows\release.yml).
  Needs Rust (https://rustup.rs), Python 3 and the Android SDK.
#>
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot
$dist = Join-Path $root 'dist'
New-Item -ItemType Directory -Force $dist | Out-Null
$python = if ($env:OS -eq 'Windows_NT') { 'python' } else { 'python3' }

# [workspace.package]'s version, as the release reads it
$manifest = Join-Path $root 'agent-rs/Cargo.toml'
$section = ''
$version = $null
foreach ($line in Get-Content -LiteralPath $manifest) {
    if ($line -match '^\s*\[([^\]]+)\]') { $section = $Matches[1].Trim() }
    elseif (-not $version -and $section -eq 'workspace.package' -and $line -match '^\s*version\s*=\s*"([^"]+)"') { $version = $Matches[1] }
}
if (-not $version) { throw "No [workspace.package] version in $manifest" }

$os = if ($env:OS -eq 'Windows_NT') { 'win' } elseif ($IsMacOS) { 'osx' } else { 'linux' }
$arch = switch ("$([Runtime.InteropServices.RuntimeInformation]::OSArchitecture)") {
    'X64' { 'x64' }
    'Arm64' { 'arm64' }
    default { throw "Unsupported CPU: $_" }
}
$rid = "$os-$arch"
$exe = if ($os -eq 'win') { 'PhoneBoxAgent.exe' } else { 'PhoneBoxAgent' }

Write-Host "==> Android APK ($version)"
Push-Location (Join-Path $root 'android')
try { .\gradlew.bat assembleRelease --console=plain -q } finally { Pop-Location }
if ($LASTEXITCODE) { throw 'Android release build failed' }
Copy-Item (Join-Path $root 'android\app\build\outputs\apk\release\app-release.apk') (Join-Path $dist "PhoneBox-$version.apk") -Force

Write-Host "==> Agent $rid ($version)"
# cargo builds into CARGO_TARGET_DIR (relative to the current folder) or agent-rs\target; passed on as an absolute
# path, it puts the build where this script looks for it, whatever a cargo config file says
$target = if ($env:CARGO_TARGET_DIR) { $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($env:CARGO_TARGET_DIR) } else { Join-Path $root 'agent-rs/target' }
$oldTarget = $env:CARGO_TARGET_DIR
# in agent-rs, so cargo uses its rust-toolchain.toml and .cargo\config.toml (which links the C runtime in on Windows)
Push-Location (Join-Path $root 'agent-rs')
# Windows PowerShell 5.1 turns cargo's progress lines on a redirected stderr into errors under 'Stop'
$ErrorActionPreference = 'Continue'
try {
    $env:CARGO_TARGET_DIR = $target
    cargo build --release --locked --manifest-path $manifest -p phonebox-agent
    $code = $LASTEXITCODE
} finally {
    $ErrorActionPreference = 'Stop'
    $env:CARGO_TARGET_DIR = $oldTarget
    Pop-Location
}
if ($code) { throw "build failed for $rid (cargo exit code $code)" }

# the zip holds exactly the executable and the notices, as the release's (tools/zip-agent.py keeps the Unix mode)
$stage = Join-Path ([IO.Path]::GetTempPath()) ("phonebox-agent-$version-$rid-" + [guid]::NewGuid().ToString('N'))
try {
    New-Item -ItemType Directory -Path $stage | Out-Null
    Copy-Item -LiteralPath (Join-Path $target "release/$exe"), (Join-Path $root 'agent-rs/THIRD-PARTY-NOTICES.txt') -Destination $stage
    & $python (Join-Path $root 'tools/zip-agent.py') $stage (Join-Path $dist "PhoneBoxAgent-$version-$rid.zip")
    if ($LASTEXITCODE) { throw "zipping the agent failed for $rid" }
} finally {
    Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue
}

Get-ChildItem $dist | Select-Object Name, @{ n = 'MB'; e = { [math]::Round($_.Length / 1MB, 1) } }
