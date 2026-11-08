#Requires -Version 7.2
[CmdletBinding()]
param(
    [ValidatePattern('^v[0-9]+\.[0-9]+\.[0-9]+$')]
    [string] $Tag,
    [ValidateRange(1, 2147483647)]
    [int] $AddonPatch,
    [string] $OutputDirectory = (Join-Path $PSScriptRoot 'dist'),
    [string] $SourceRepository = 'https://github.com/Swofty-Developments/SwoftyLauncher.git',
    [string] $ReleaseZip,
    [string] $RustToolchain,
    [switch] $AddonOnly,
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

function Invoke-Download(
    [string] $Uri,
    [string] $Path,
    [hashtable] $Headers = @{},
    [int] $MaxAttempts = 5
) {
    for ($attempt = 1; $attempt -le $MaxAttempts; $attempt++) {
        Remove-Item -LiteralPath $Path -Force -ErrorAction SilentlyContinue
        try {
            Invoke-WebRequest -Uri $Uri -OutFile $Path -Headers $Headers -TimeoutSec 300
            return
        } catch {
            $status = $null
            if ($_.Exception.Response) {
                try { $status = [int]$_.Exception.Response.StatusCode } catch {}
            }
            $retryable = $null -eq $status -or $status -in @(408, 429, 500, 502, 503, 504)
            if (!$retryable -or $attempt -eq $MaxAttempts) {
                throw
            }
            $delay = [Math]::Min(30, [int][Math]::Pow(2, $attempt))
            $reason = if ($null -ne $status) { "HTTP $status" } else { $_.Exception.Message }
            Write-Warning "Download failed ($reason), attempt $attempt of $MaxAttempts. Retrying in $delay seconds."
            Start-Sleep -Seconds $delay
        }
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
$addonTag = $Tag
$addonVersion = $version
if ($AddonPatch) {
    $addonTag = "${Tag}_patch$AddonPatch"
    $addonVersion = "$version+patch.$AddonPatch"
}
$output = Join-Path $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputDirectory) $addonTag
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
        $upstreamRelease = Invoke-RestMethod -Uri "https://api.github.com/repos/Swofty-Developments/SwoftyLauncher/releases/tags/$Tag" -TimeoutSec 30
        if ($upstreamRelease.tag_name -cne $Tag -or $upstreamRelease.draft -or $upstreamRelease.prerelease) {
            throw "GitHub did not return the expected stable release $Tag."
        }
        $checksumAssets = @($upstreamRelease.assets | Where-Object name -CEQ 'SHA256SUMS')
        $packageAssets = @($upstreamRelease.assets | Where-Object name -CEQ "$asset.zip")
        if ($checksumAssets.Count -ne 1 -or $packageAssets.Count -ne 1) {
            throw "Release $Tag must contain exactly one SHA256SUMS and one $asset.zip asset."
        }
        $ReleaseZip = Join-Path $work "$asset.zip"
        $sums = Join-Path $work 'upstream-SHA256SUMS'
        Invoke-Download -Uri $checksumAssets[0].url -Path $sums -Headers @{ Accept = 'application/octet-stream' }
        $pattern = '^([a-fA-F0-9]{64}) [ *]' + [regex]::Escape("$asset.zip") + '$'
        $expected = Select-String -LiteralPath $sums -Pattern $pattern
        if (!$expected -or @($expected).Count -ne 1) {
            throw "No unique checksum found for $asset.zip."
        }
        $expectedHash = $expected.Matches[0].Groups[1].Value
        Invoke-Download -Uri $packageAssets[0].url -Path $ReleaseZip -Headers @{ Accept = 'application/octet-stream' }
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
        Invoke-Checked cargo ($cargoArgs + @('test') + $buildArgs)
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
        addon_tag = $addonTag
        addon_version = $addonVersion
        upstream_commit = $commit
        upstream_windows_zip_sha256 = $zipHash
        cli_patch_sha256 = (Get-FileHash -LiteralPath $patch -Algorithm SHA256).Hash.ToLowerInvariant()
        cli_version = $label
    }
    $info | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $bundle 'BUILD_INFO.json') -Encoding utf8
    $packages = @()
    if (!$AddonOnly) {
        $runnerZip = Join-Path $staging "custom-play-runner-$addonTag-windows-x64.zip"
        [IO.Compression.ZipFile]::CreateFromDirectory($bundle, $runnerZip)
        $packages += $runnerZip
    }

    Copy-Item -LiteralPath (Join-Path $binaries 'custom-play-helper.exe') -Destination $bin
    $manifest = Get-Content -LiteralPath (Join-Path $addonSource 'swofty.json') -Raw | ConvertFrom-Json
    $manifest.version = $addonVersion
    $manifest.compat.launcher = ">=$version"
    $manifest | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath (Join-Path $bundle 'swofty.json') -Encoding utf8
    $addonZip = Join-Path $staging "local.custom-play-$($addonTag.Substring(1))-windows-x64.swoftyaddon"
    [IO.Compression.ZipFile]::CreateFromDirectory($bundle, $addonZip)
    $packages += $addonZip
    Get-FileHash -LiteralPath $packages -Algorithm SHA256 | ForEach-Object {
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
