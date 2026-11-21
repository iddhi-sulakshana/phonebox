# Get-NewestRelease from setup.ps1 (-Pre) with GitHub stubbed: the API, then the release feed when the API refuses.
$ErrorActionPreference = 'Stop'
$setup = Join-Path (Join-Path $PSScriptRoot '..') 'setup.ps1'
$ast = [System.Management.Automation.Language.Parser]::ParseFile($setup, [ref]$null, [ref]$null)
$definition = $ast.FindAll({
        param($node)
        $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Get-NewestRelease'
    }, $true) | Select-Object -First 1
if (-not $definition) { throw 'setup.ps1 has no Get-NewestRelease' }
. ([scriptblock]::Create($definition.Extent.Text))

$Repo = 'owner/repo'
$script:apiAnswer = $null
function Invoke-RestMethod([switch]$UseBasicParsing, $Uri, $Headers, $TimeoutSec) {
    if ($null -eq $script:apiAnswer) { throw 'The remote server returned an error: (403) Forbidden.' }
    return $script:apiAnswer
}
function Invoke-WebRequest([switch]$UseBasicParsing, $Uri, $Headers, $TimeoutSec) {
    if ($Uri -ne 'https://github.com/owner/repo/releases.atom') { throw "unexpected request: $Uri" }
    return [pscustomobject]@{ Content = '<feed><entry><link rel="alternate" type="text/html" href="https://github.com/owner/repo/releases/tag/v1.0.0-alpha.2"/></entry><entry><link rel="alternate" type="text/html" href="https://github.com/owner/repo/releases/tag/v1.0.0-alpha.1"/></entry></feed>' }
}

$script:apiAnswer = @([pscustomobject]@{ tag_name = 'v1.0.0-alpha.3' })
$got = Get-NewestRelease
if ($got -ne 'v1.0.0-alpha.3') { throw "the API's newest release: expected v1.0.0-alpha.3, got $got" }

# the API's hourly limit is used up
$script:apiAnswer = $null
$got = Get-NewestRelease
if ($got -ne 'v1.0.0-alpha.2') { throw "the feed's newest release: expected v1.0.0-alpha.2, got $got" }
'PASS: Get-NewestRelease'
