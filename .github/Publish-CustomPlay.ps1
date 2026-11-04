#Requires -Version 7.2
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Invoke-Gh([string[]] $Arguments) {
    $result = & gh @Arguments
    if ($LASTEXITCODE -ne 0) { throw "GitHub CLI failed (exit $LASTEXITCODE): $($Arguments -join ' ')" }
    $result
}

function Get-GitHubItems([string] $Endpoint) {
    $json = (Invoke-Gh @('api', '--paginate', '--slurp', "${Endpoint}?per_page=100")) -join "`n"
    foreach ($page in (ConvertFrom-Json -InputObject $json -NoEnumerate)) {
        foreach ($item in $page) { $item }
    }
}

function Get-ReleasePlan([string] $UpstreamTag, [string] $Commit, [object[]] $Releases, [object[]] $Tags) {
    $addonPattern = '^v[0-9]+\.[0-9]+\.[0-9]+(?:_patch[1-9][0-9]*)?$'
    $basePattern = '^' + [regex]::Escape($UpstreamTag) + '(?:_patch(?<patch>[1-9][0-9]*))?$'
    $tagCommits = @{}
    foreach ($tag in $Tags) { $tagCommits[$tag.name] = $tag.commit.sha }
    foreach ($release in $Releases) {
        if (!$release.draft -and !$release.prerelease -and $release.tag_name -cmatch $addonPattern -and
            $tagCommits[$release.tag_name] -eq $Commit) {
            return @{ Published = $release.tag_name }
        }
    }

    $highest = -1
    foreach ($name in @($tagCommits.Keys) + @($Releases | ForEach-Object tag_name)) {
        if ($name -cmatch $basePattern) {
            $patch = if ($Matches.ContainsKey('patch')) { [int]$Matches.patch } else { 0 }
            $highest = [Math]::Max($highest, $patch)
        }
    }
    $lastTag = if ($highest -gt 0) { "${UpstreamTag}_patch$highest" } else { $UpstreamTag }
    $draft = $Releases | Where-Object {
        $_.draft -and !$_.prerelease -and $_.tag_name -ceq $lastTag -and $_.target_commitish -eq $Commit -and
            (!$tagCommits.ContainsKey($lastTag) -or $tagCommits[$lastTag] -eq $Commit)
    } | Select-Object -First 1
    if ($draft) { return @{ Tag = $lastTag; Patch = $highest; DraftId = $draft.id } }
    if ($highest -eq [int]::MaxValue) { throw "No patch numbers remain for $UpstreamTag." }
    $next = $highest + 1
    $nextTag = if ($next) { "${UpstreamTag}_patch$next" } else { $UpstreamTag }
    return @{ Tag = $nextTag; Patch = $next; DraftId = $null }
}

$commit = $env:GITHUB_SHA
$message = (& git log -1 --format=%B $commit) -join "`n"
if ($LASTEXITCODE -ne 0) { throw "Could not read commit $commit." }
$prefix = 'releasecommit:'
if (!$message.StartsWith($prefix, [StringComparison]::Ordinal)) {
    Write-Host 'The tip commit does not have the exact release prefix.'
    return
}

$upstream = Invoke-Gh @('api', 'repos/Swofty-Developments/SwoftyLauncher/releases/latest') | ConvertFrom-Json
$upstreamTag = $upstream.tag_name
if ($upstream.draft -or $upstream.prerelease -or $upstreamTag -cnotmatch '^v[0-9]+\.[0-9]+\.[0-9]+$') {
    throw "Expected a stable upstream version, got $upstreamTag."
}
$repo = "repos/$env:GITHUB_REPOSITORY"
$plan = Get-ReleasePlan $upstreamTag $commit @(Get-GitHubItems "$repo/releases") @(Get-GitHubItems "$repo/tags")
if ($plan['Published']) {
    Write-Host "Commit $commit already has release $($plan.Published)."
    return
}

$tag = $plan.Tag
$tempRoot = (Resolve-Path -LiteralPath $env:RUNNER_TEMP).ProviderPath
$scratch = Join-Path $tempRoot ('custom-play-release-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $scratch | Out-Null
$oldCargoHome = $env:CARGO_HOME
$oldToken = $env:GH_TOKEN
try {
    $env:CARGO_HOME = Join-Path $scratch 'cargo'
    $output = Join-Path $scratch 'dist'
    $build = @{ Tag = $upstreamTag; AddonOnly = $true; OutputDirectory = $output }
    if ($plan.Patch) { $build.AddonPatch = $plan.Patch }
    $env:GH_TOKEN = $null
    try {
        & "$PSScriptRoot/../Build-CustomPlay.ps1" @build
    } finally {
        $env:GH_TOKEN = $oldToken
    }

    $name = "local.custom-play-$($tag.Substring(1))-windows-x64.swoftyaddon"
    $asset = Join-Path $output "$tag/$name"
    $digest = 'sha256:' + (Get-FileHash -LiteralPath $asset -Algorithm SHA256).Hash.ToLowerInvariant()
    $notesFile = Join-Path $scratch 'notes.md'
    $notes = $message.Substring($prefix.Length).Trim()
    if (!$notes) { $notes = "Custom Play $tag for Swofty Launcher $upstreamTag." }
    Set-Content -LiteralPath $notesFile -Value $notes -Encoding utf8

    $draftId = $plan.DraftId
    if (!$draftId) {
        $draft = Invoke-Gh @('api', '--method', 'POST', "$repo/releases", '-f', "tag_name=$tag",
            '-f', "target_commitish=$commit", '-f', "name=$tag", '-F', "body=@$notesFile", '-F', 'draft=true') | ConvertFrom-Json
        $draftId = $draft.id
    }
    $draft = Invoke-Gh @('api', "$repo/releases/$draftId") | ConvertFrom-Json
    if (!$draft.draft -or $draft.tag_name -cne $tag -or $draft.target_commitish -ne $commit) {
        throw "Release $tag is no longer a draft for commit $commit."
    }
    Invoke-Gh @('release', 'upload', $tag, $asset, '--clobber', '--repo', $env:GITHUB_REPOSITORY)
    $uploaded = @(Get-GitHubItems "$repo/releases/$draftId/assets")
    if ($uploaded.Count -ne 1 -or $uploaded[0].name -cne $name -or $uploaded[0].digest -ne $digest) {
        throw "The uploaded addon is incomplete or its checksum does not match. $tag remains a draft."
    }
    $existingTag = Get-GitHubItems "$repo/tags" | Where-Object name -CEQ $tag | Select-Object -First 1
    if ($existingTag -and $existingTag.commit.sha -ne $commit) {
        throw "Git tag $tag already points to a different commit. The release remains a draft."
    }
    $published = Invoke-Gh @('api', '--method', 'PATCH', "$repo/releases/$draftId", '-F', 'draft=false',
        '-f', 'make_latest=true') | ConvertFrom-Json
    Write-Host "Published $($published.html_url)"
} finally {
    $env:CARGO_HOME = $oldCargoHome
    $env:GH_TOKEN = $oldToken
    $resolved = (Resolve-Path -LiteralPath $scratch).ProviderPath
    if ($resolved.StartsWith($tempRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
        Remove-Item -LiteralPath $resolved -Recurse -Force
    }
}
