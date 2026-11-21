$ErrorActionPreference = 'Stop'
$fixture = Join-Path ([IO.Path]::GetTempPath()) ('phonebox-build-test-' + [guid]::NewGuid().ToString('N'))
$scriptPath = Join-Path $fixture 'scripts/build.ps1'
$manifest = Join-Path $fixture 'agent-rs/Cargo.toml'
$notices = Join-Path $fixture 'agent-rs/THIRD-PARTY-NOTICES.txt'
$zipper = Join-Path $fixture 'tools/zip-agent.py'
$wrapper = Join-Path $fixture 'android/gradlew.bat'
$apk = Join-Path $fixture 'android/app/build/outputs/apk/release/app-release.apk'
# build.ps1 stages the agent in the temp folder: one of the fixture's, so a stage left behind is seen
$temp = Join-Path $fixture 'temp'
foreach ($path in @($scriptPath, $manifest, $zipper, $apk, (Join-Path $temp 'x'))) {
    New-Item -ItemType Directory -Path (Split-Path -Parent $path) -Force | Out-Null
}
Copy-Item -LiteralPath (Join-Path $PSScriptRoot '../scripts/build.ps1') -Destination $scriptPath
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'zip-agent.py') -Destination $zipper
# the version is [workspace.package]'s, not the first version line of the file
[IO.File]::WriteAllText($manifest, "[workspace]`nmembers = []`n`n[workspace.metadata.fixture]`nversion = `"9.9.9`"`n`n[workspace.package]`nversion = `"0.0.1-alpha.2`"`nedition = `"2024`"`n")
[IO.File]::WriteAllText($notices, 'fixture notices')
[IO.File]::WriteAllText($apk, 'fixture apk')
if ($env:OS -eq 'Windows_NT') {
    [IO.File]::WriteAllText($wrapper, "@exit /b 0`r`n")
} else {
    [IO.File]::WriteAllText($wrapper, "#!/bin/sh`nexit 0`n")
    chmod +x $wrapper
}

$oldTargetDir = $env:CARGO_TARGET_DIR
$oldTemp = @{ TMP = $env:TMP; TEMP = $env:TEMP; TMPDIR = $env:TMPDIR }
$env:TMP = $temp; $env:TEMP = $temp; $env:TMPDIR = $temp
$env:CARGO_TARGET_DIR = Join-Path $fixture 'cargo target'
$exe = if ($env:OS -eq 'Windows_NT') { 'PhoneBoxAgent.exe' } else { 'PhoneBoxAgent' }
$fakeBuild = [pscustomobject]@{ Fail = $false; Arguments = @() }
# the agent is the Rust build: a stray dotnet call fails at once
function dotnet { throw 'dotnet is not used any more' }
# a fake `cargo build` that writes only into the fixture's CARGO_TARGET_DIR, never into the real agent-rs/target
function cargo {
    if (-not "$env:CARGO_TARGET_DIR".StartsWith($fixture)) { throw "cargo stub: CARGO_TARGET_DIR is '$env:CARGO_TARGET_DIR', not a folder of the test" }
    $fakeBuild.Arguments = @($args)
    $release = Join-Path $env:CARGO_TARGET_DIR 'release'
    New-Item -ItemType Directory -Path $release -Force | Out-Null
    [IO.File]::WriteAllText((Join-Path $release $exe), 'agent artifact')
    $global:LASTEXITCODE = if ($fakeBuild.Fail) { 7 } else { 0 }
}

try {
    # the manifest path is compared by the fixture's folder name: Windows may spell the temp folder in its 8.3 form
    $expected = "build --release --locked --manifest-path *$(Split-Path $fixture -Leaf)?agent-rs?Cargo.toml -p phonebox-agent"
    foreach ($failing in @($false, $true)) {
        $fakeBuild.Fail = $failing
        $fakeBuild.Arguments = @()
        $message = ''
        try { & $scriptPath | Out-Null } catch { $message = $_.Exception.Message }
        if (($fakeBuild.Arguments -join ' ') -notlike $expected) { throw "Unexpected cargo arguments: $($fakeBuild.Arguments -join ' ')" }
        if (-not $failing) {
            if ($message) { throw $message }
            $zips = @(Get-ChildItem -LiteralPath (Join-Path $fixture 'dist') -Filter 'PhoneBoxAgent-*.zip')
            if ($zips.Count -ne 1 -or $zips[0].Name -notmatch '^PhoneBoxAgent-0\.0\.1-alpha\.2-(win|linux|osx)-(x64|arm64)\.zip$') {
                throw "Expected one agent archive for this computer, got: $($zips.Name -join ', ')"
            }
            Add-Type -AssemblyName System.IO.Compression.FileSystem
            $archive = [IO.Compression.ZipFile]::OpenRead($zips[0].FullName)
            try { $entries = @($archive.Entries.FullName | Sort-Object) } finally { $archive.Dispose() }
            if (($entries -join ',') -ne (@($exe, 'THIRD-PARTY-NOTICES.txt' | Sort-Object) -join ',')) { throw "Unexpected archive entries: $($entries -join ', ')" }
            if (-not (Test-Path -LiteralPath (Join-Path $fixture 'dist/PhoneBox-0.0.1-alpha.2.apk'))) { throw 'The APK was not copied to dist' }
        } elseif ($message -notmatch '^build failed for ') {
            throw "Expected a build failure, got: $message"
        }
        if (@(Get-ChildItem -LiteralPath $temp -Force -Filter 'phonebox-*').Count) { throw 'The agent stage was left behind' }
    }
    'PASS: successful and failed local packaging both clean their agent stages'
} finally {
    $env:CARGO_TARGET_DIR = $oldTargetDir
    foreach ($name in $oldTemp.Keys) { [Environment]::SetEnvironmentVariable($name, $oldTemp[$name]) }
    [IO.Directory]::Delete($fixture, $true)
}

# A failing fake build above leaves $LASTEXITCODE non-zero; the runner's pwsh wrapper would report that as the result.
exit 0
