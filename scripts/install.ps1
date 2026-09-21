<#
.SYNOPSIS
  Installs omega for the current user: the binary in a directory that
  stays, that directory on PATH, and the search model.

.DESCRIPTION
  Run it from an unpacked release archive (the binary beside this script is the
  one installed), or on its own, in which case the release is downloaded with
  the GitHub CLI -- the repository is private, so `gh auth login` must have
  been done once:

    gh api repos/gyxoBka/omega-seek/contents/scripts/install.ps1 -H "Accept: application/vnd.github.raw" | Out-String | iex

  Running it again updates the binary and changes nothing else.

.PARAMETER Uninstall
  Takes omega out of the coding agents, removes the binary and the PATH
  entry. -Purge also removes the model and the index caches.
#>
[CmdletBinding()]
param(
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\omega'),
    [string]$Repo = $(if ($env:OMEGA_REPO) { $env:OMEGA_REPO } else { 'gyxoBka/omega-seek' }),
    [string]$Version = 'latest',
    [switch]$NoPath,
    [switch]$NoModel,
    [switch]$Uninstall,
    [switch]$Purge
)

$ErrorActionPreference = 'Stop'
$Asset = 'omega-x86_64-pc-windows-msvc.zip'
$Exe = Join-Path $InstallDir 'omega.exe'

function Get-UserPathEntries {
    $current = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ([string]::IsNullOrEmpty($current)) { return @() }
    return @($current.Split(';') | Where-Object { $_ -ne '' })
}

function Test-SameDirectory([string]$a, [string]$b) {
    return $a.TrimEnd('\') -ieq $b.TrimEnd('\')
}

function Add-ToUserPath([string]$dir) {
    $entries = Get-UserPathEntries
    if ($entries | Where-Object { Test-SameDirectory $_ $dir }) {
        Write-Host "  PATH         already contains $dir"
        return
    }
    [Environment]::SetEnvironmentVariable('Path', (@($entries) + $dir) -join ';', 'User')
    $env:Path = "$env:Path;$dir"
    Write-Host "  PATH         added $dir (new terminals see it)"
}

function Remove-FromUserPath([string]$dir) {
    $entries = Get-UserPathEntries
    $kept = @($entries | Where-Object { -not (Test-SameDirectory $_ $dir) })
    if ($kept.Count -eq $entries.Count) { return }
    [Environment]::SetEnvironmentVariable('Path', $kept -join ';', 'User')
    Write-Host "  PATH         removed $dir"
}

if ($Uninstall) {
    Write-Host "`n  omega uninstall`n"
    if (Test-Path $Exe) {
        & $Exe uninstall --yes
    }
    if (-not $NoPath) { Remove-FromUserPath $InstallDir }
    # Only the files omega put there; the directory goes when nothing else is in it.
    foreach ($file in $Exe, "$Exe.old") {
        if (Test-Path $file) {
            Remove-Item -Force $file -Confirm:$false
            Write-Host "  binary       removed $file"
        }
    }
    if ((Test-Path $InstallDir) -and -not (Get-ChildItem -Force $InstallDir)) {
        Remove-Item -Force $InstallDir -Confirm:$false
    }
    if ($Purge) {
        foreach ($dir in 'models', 'index') {
            $path = Join-Path $env:LOCALAPPDATA "omega\$dir"
            if (Test-Path $path) {
                Remove-Item -Recurse -Force $path -Confirm:$false
                Write-Host "  data         removed $path"
            }
        }
    }
    Write-Host ''
    return
}

Write-Host "`n  omega install`n"

# The binary: the one beside this script, else the release asset.
$source = $null
if ($PSScriptRoot) {
    foreach ($candidate in (Join-Path $PSScriptRoot 'omega.exe'), (Join-Path $PSScriptRoot '..\omega.exe')) {
        if (Test-Path $candidate) { $source = (Resolve-Path $candidate).Path; break }
    }
}
$staging = $null
if (-not $source) {
    if (-not (Get-Command gh -ErrorAction SilentlyContinue)) {
        throw "No omega.exe beside this script and no GitHub CLI to download one. Install gh (https://cli.github.com), run 'gh auth login', and try again -- or download $Asset from the repository's Releases page, unpack it, and run install.ps1 from there."
    }
    $staging = Join-Path ([IO.Path]::GetTempPath()) "omega-install-$PID"
    New-Item -ItemType Directory -Force $staging | Out-Null
    $tag = @()
    if ($Version -ne 'latest') { $tag = @($Version) }
    Write-Host "  download     $Asset ($Version) from $Repo"
    & gh release download @tag --repo $Repo --pattern $Asset --dir $staging --clobber
    if ($LASTEXITCODE -ne 0) { throw "gh could not download $Asset from $Repo" }
    Expand-Archive -Force (Join-Path $staging $Asset) $staging
    $source = (Get-ChildItem -Recurse $staging -Filter 'omega.exe' | Select-Object -First 1).FullName
    if (-not $source) { throw "$Asset holds no omega.exe" }
}

New-Item -ItemType Directory -Force $InstallDir | Out-Null
if (-not (Test-SameDirectory (Split-Path $source) $InstallDir)) {
    # A running MCP server keeps the old binary open. Windows lets an open file
    # be renamed but not overwritten, so the old one steps aside.
    if (Test-Path $Exe) {
        $aside = "$Exe.old"
        if (Test-Path $aside) { Remove-Item -Force $aside -ErrorAction SilentlyContinue -Confirm:$false }
        Rename-Item $Exe $aside
    }
    Copy-Item $source $Exe
    Remove-Item -Force "$Exe.old" -ErrorAction SilentlyContinue -Confirm:$false
}
Write-Host "  binary       $Exe"
if ($staging) { Remove-Item -Recurse -Force $staging -ErrorAction SilentlyContinue -Confirm:$false }

if (-not $NoPath) { Add-ToUserPath $InstallDir }

if (-not $NoModel) {
    Write-Host "  model        installing (32 MB, once)"
    & $Exe model install | Out-Null
    if ($LASTEXITCODE -ne 0) { Write-Warning "The model did not install; search stays lexical until 'omega model install' succeeds." }
}

Write-Host "`n  Done. Next: connect it to your coding agents with`n`n      omega install`n"
