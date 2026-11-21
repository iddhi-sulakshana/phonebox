<#
.SYNOPSIS
  Dispatches the gated release workflow (.github/workflows/release.yml) on GitHub for a pushed branch; the release is
  created as a draft. Then follow docs/RELEASING.md.

.EXAMPLE
  .\scripts\release.ps1                # the current branch, which must be pushed with HEAD at its tip
  .\scripts\release.ps1 -Ref main      # another branch (HEAD must still be its pushed tip)
  .\scripts\release.ps1 -Final         # a final version such as 1.0.0 (ask the owner first)
#>
param(
    [string]$Ref,     # the branch to release; the current branch by default
    [switch]$Final    # allow a final version: check-release.py --final and the workflow's final=true
)
$ErrorActionPreference = 'Stop'
$repo = 'iddhi-sulakshana/phonebox'
$root = Split-Path $PSScriptRoot
$python = if ($env:OS -eq 'Windows_NT') { 'python' } else { 'python3' }
if (-not $Ref) {
    $Ref = git -C $root symbolic-ref --short -q HEAD
    if ($LASTEXITCODE -or -not $Ref) { throw 'HEAD is not on a branch: pass -Ref <branch>' }
}
$check = @((Join-Path $root 'tools/check-release.py'))
if ($Final) { $check += '--final' }
& $python @check
if ($LASTEXITCODE) { throw 'Release metadata check failed' }
if (git -C $root status --porcelain) { throw 'Commit and push the reviewed changes first' }
$local = git -C $root rev-parse HEAD
# the fork's copy of the branch, whatever this clone's remotes are called
$remote = gh api "repos/$repo/commits/$Ref" --jq .sha
if ($LASTEXITCODE -or $local -ne $remote) { throw "HEAD must match $Ref as pushed to $repo" }
$inputs = @('-f', 'draft=true')
if ($Final) { $inputs += @('-f', 'final=true') }
gh workflow run release.yml --repo $repo --ref $Ref @inputs
if ($LASTEXITCODE) { throw 'Release workflow dispatch failed' }
Write-Host "Release of $Ref dispatched. Follow the workflow and complete docs/RELEASING.md."
