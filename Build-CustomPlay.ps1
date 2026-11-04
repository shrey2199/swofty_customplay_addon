#Requires -Version 7.2
[CmdletBinding()]
param(
    [ValidatePattern('^v[0-9]+\.[0-9]+\.[0-9]+$')]
    [string] $Tag,
    [string] $OutputDirectory = (Join-Path $PSScriptRoot 'dist'),
    [string] $SourceRepository = 'https://github.com/Swofty-Developments/SwoftyLauncher.git',
    [string] $ReleaseZip,
    [string] $RustToolchain,
    [switch] $CheckPatch
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$project = 'https://github.com/Swofty-Developments/SwoftyLauncher'
$patch = Join-Path $PSScriptRoot 'patches/custom-play-cli.patch'
$addonSource = Join-Path $PSScriptRoot 'addon'
$target = 'x86_64-pc-windows-msvc'

function Invoke-Checked([string] $Command, [string[]] $Arguments) {
    & $Command @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$Command exited with code $LASTEXITCODE ($($Arguments -join ' '))"
    }
}

function Assert-File([string] $Path) {
    if (!(Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "Required file missing: $Path"
    }
}

Get-Command git -CommandType Application | Out-Null
if (!$CheckPatch) {
    if (!$IsWindows -or [Runtime.InteropServices.RuntimeInformation]::OSArchitecture -ne 'X64') {
        throw 'The release build and process tests require x64 Windows.'
    }
    Get-Command cargo -CommandType Application | Out-Null
}
if (!$Tag) {
    if ($ReleaseZip -or $SourceRepository -ne "$project.git") {
        throw 'Specify -Tag when using a local source repository or release ZIP.'
    }
    $release = Invoke-RestMethod 'https://api.github.com/repos/Swofty-Developments/SwoftyLauncher/releases/latest'
    $Tag = $release.tag_name
    if ($release.draft -or $release.prerelease -or $Tag -cnotmatch '^v[0-9]+\.[0-9]+\.[0-9]+$') {
        throw "Expected a stable vMAJOR.MINOR.PATCH release tag, got $Tag."
    }
}
$version = $Tag.Substring(1)
$output = Join-Path $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputDirectory) $Tag
if (!$CheckPatch -and (Test-Path -LiteralPath $output)) {
    throw "Output already exists at $output. Choose another -OutputDirectory or move the old build."
}
if ($ReleaseZip) {
    $ReleaseZip = (Resolve-Path -LiteralPath $ReleaseZip).ProviderPath
}
$localSource = Test-Path -LiteralPath $SourceRepository -PathType Container
if ($localSource) {
    $SourceRepository = (Resolve-Path -LiteralPath $SourceRepository).ProviderPath
}

$work = Join-Path $PSScriptRoot ('.work/' + [guid]::NewGuid().ToString('N'))
$source = Join-Path $work 'upstream'
New-Item -ItemType Directory -Path $source -Force | Out-Null
try {
    Write-Host "Fetching Swofty Launcher $Tag source"
    if ($localSource) {
        $commit = Invoke-Checked git @('-C', $SourceRepository, 'rev-parse', '--verify', "refs/tags/$Tag^{commit}")
        $snapshot = Join-Path $work 'source.zip'
        Invoke-Checked git @('-C', $SourceRepository, 'archive', '--format=zip', "--output=$snapshot", $commit)
        [IO.Compression.ZipFile]::ExtractToDirectory($snapshot, $source)
        Invoke-Checked git @('-C', $source, 'init', '--quiet')
    } else {
        Invoke-Checked git @('-C', $source, 'init', '--quiet')
        Invoke-Checked git @('-C', $source, 'fetch', '--quiet', '--depth=1', '--no-tags', '--', $SourceRepository, "refs/tags/$Tag")
        $commit = Invoke-Checked git @('-C', $source, 'rev-parse', 'FETCH_HEAD^{commit}')
        Invoke-Checked git @('-C', $source, 'checkout', '--quiet', '--detach', $commit)
    }
    Write-Host "Checking CLI patch against $commit"
    Invoke-Checked git @('-C', $source, 'apply', '--check', $patch)
    Invoke-Checked git @('-C', $source, 'apply', $patch)
    if ($CheckPatch) {
        Write-Host "Patch applied successfully to $Tag ($commit)."
        return
    }

    $asset = "swofty-launcher-$Tag-windows-x86_64"
    $expectedHash = $null
    if (!$ReleaseZip) {
        Write-Host "Downloading the official Windows package for $Tag"
        $download = "$project/releases/download/$Tag"
        $ReleaseZip = Join-Path $work "$asset.zip"
        $sums = Join-Path $work 'upstream-SHA256SUMS'
        Invoke-WebRequest -Uri "$download/SHA256SUMS" -OutFile $sums
        $pattern = '^([a-fA-F0-9]{64}) [ *]' + [regex]::Escape("$asset.zip") + '$'
        $expected = Select-String -LiteralPath $sums -Pattern $pattern
        if (!$expected -or @($expected).Count -ne 1) {
            throw "No unique checksum found for $asset.zip."
        }
        $expectedHash = $expected.Matches[0].Groups[1].Value
        Invoke-WebRequest -Uri "$download/$asset.zip" -OutFile $ReleaseZip
    }
    $zipHash = (Get-FileHash -LiteralPath $ReleaseZip -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($expectedHash -and $zipHash -ne $expectedHash) {
        throw "SHA-256 mismatch for $asset.zip."
    }

    $extracted = Join-Path $work 'release'
    [IO.Compression.ZipFile]::ExtractToDirectory($ReleaseZip, $extracted)
    $official = Join-Path $extracted $asset
    Assert-File (Join-Path $official 'loader/version.dll')
    Assert-File (Join-Path $official 'dxc/bin/dxc.exe')
    Assert-File (Join-Path $official 'LICENSE')
    Assert-File (Join-Path $official 'THIRD_PARTY.md')
    if (!(Test-Path -LiteralPath (Join-Path $official 'loader/swofty') -PathType Container)) {
        throw 'The upstream ZIP is missing its loader/swofty payload.'
    }

    $cargoArgs = @()
    if ($RustToolchain) { $cargoArgs += "+$RustToolchain" }
    $cargoTarget = Join-Path $work 'target'
    $buildArgs = @('--release', '--locked', '--target', $target, '--target-dir', $cargoTarget)
    $label = "${version}_custom"
    $oldLabel = $env:SWOFTY_BUILD_LABEL
    $env:SWOFTY_BUILD_LABEL = $label
    Push-Location $source
    try {
        Write-Host 'Building the patched CLI and running its Custom Play tests'
        Invoke-Checked cargo ($cargoArgs + @('build', '-p', 'swofty-cli', '--bin', 'swofty') + $buildArgs)
        Invoke-Checked cargo ($cargoArgs + @('test', '-p', 'swofty-cli', '--bin', 'swofty') + $buildArgs +
            @('run::custom_tests::', '--', '--test-threads=1'))
        $binaries = Join-Path $cargoTarget "$target/release"
        $cli = Join-Path $binaries 'swofty.exe'
        if ((Invoke-Checked $cli @('--version')) -cne "swofty $label") {
            throw "The CLI did not report the expected version $label."
        }
    } finally {
        Pop-Location
        $env:SWOFTY_BUILD_LABEL = $oldLabel
    }

    Push-Location $addonSource
    try {
        Write-Host 'Building the Custom Play addon helper'
        Invoke-Checked cargo ($cargoArgs + @('build') + $buildArgs)
    } finally {
        Pop-Location
    }

    $bundle = Join-Path $work 'bundle'
    $bin = Join-Path $bundle 'bin'
    $staging = Join-Path $work 'output'
    New-Item -ItemType Directory -Path $bin, $staging | Out-Null
    Copy-Item -LiteralPath $cli -Destination $bin
    foreach ($entry in 'loader', 'dxc', 'LICENSE', 'THIRD_PARTY.md') {
        Copy-Item -LiteralPath (Join-Path $official $entry) -Destination $bundle -Recurse
    }
    $info = [ordered]@{
        upstream_repository = $project
        upstream_tag = $Tag
        upstream_commit = $commit
        upstream_windows_zip_sha256 = $zipHash
        cli_patch_sha256 = (Get-FileHash -LiteralPath $patch -Algorithm SHA256).Hash.ToLowerInvariant()
        cli_version = $label
    }
    $info | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $bundle 'BUILD_INFO.json') -Encoding utf8
    $runnerZip = Join-Path $staging "custom-play-runner-$Tag-windows-x64.zip"
    [IO.Compression.ZipFile]::CreateFromDirectory($bundle, $runnerZip)

    Copy-Item -LiteralPath (Join-Path $binaries 'custom-play-helper.exe') -Destination $bin
    $manifest = Get-Content -LiteralPath (Join-Path $addonSource 'swofty.json') -Raw | ConvertFrom-Json
    $manifest.version = $version
    $manifest.compat.launcher = ">=$version"
    $manifest | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath (Join-Path $bundle 'swofty.json') -Encoding utf8
    $addonZip = Join-Path $staging "local.custom-play-$version-windows-x64.swoftyaddon"
    [IO.Compression.ZipFile]::CreateFromDirectory($bundle, $addonZip)
    Get-FileHash -LiteralPath $runnerZip, $addonZip -Algorithm SHA256 | ForEach-Object {
        '{0}  {1}' -f $_.Hash.ToLowerInvariant(), [IO.Path]::GetFileName($_.Path)
    } | Set-Content -LiteralPath (Join-Path $staging 'SHA256SUMS') -Encoding ascii

    New-Item -ItemType Directory -Path (Split-Path -Parent $output) -Force | Out-Null
    Move-Item -LiteralPath $staging -Destination $output
    Write-Host "Build complete: $output"
} finally {
    $workRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '.work')) + [IO.Path]::DirectorySeparatorChar
    $resolvedWork = (Resolve-Path -LiteralPath $work).ProviderPath
    if ($resolvedWork.StartsWith($workRoot, [StringComparison]::OrdinalIgnoreCase)) {
        Remove-Item -LiteralPath $resolvedWork -Recurse -Force
    }
}
