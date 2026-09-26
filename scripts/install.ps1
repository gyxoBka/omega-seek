<#
.SYNOPSIS
  Installs omega for the current user: the binary in a directory that
  stays, that directory on PATH, and the search model.

.DESCRIPTION
  Run it from an unpacked release archive (the binary beside this script is the
  one installed), or on its own, in which case the release is downloaded with
  from the latest GitHub release:

    irm https://raw.githubusercontent.com/gyxoBka/omega-seek/master/scripts/install.ps1 | iex

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
    $binaries = @(Get-ChildItem -Path $InstallDir -Filter 'omega.exe*' -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -eq 'omega.exe' -or $_.Name -like 'omega.exe.old*' })
    foreach ($file in $binaries) {
        Remove-Item -Force $file.FullName -ErrorAction SilentlyContinue -Confirm:$false
        if (Test-Path $file.FullName) {
            Write-Host "  binary       $($file.FullName) is open in a running session; delete it once that session ends"
        } else {
            Write-Host "  binary       removed $($file.FullName)"
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
# What was downloaded goes whether or not the install succeeds: a failed one
# used to leave the archive and its unpacked copy in the temp directory.
try {
if (-not $source) {
    $staging = Join-Path ([IO.Path]::GetTempPath()) "omega-install-$PID"
    New-Item -ItemType Directory -Force $staging | Out-Null
    Write-Host "  download     $Asset ($Version) from $Repo"
    # A public release is a plain URL. The GitHub CLI is the way in for a
    # private fork, when it is there.
    $url = if ($Version -eq 'latest') { "https://github.com/$Repo/releases/latest/download/$Asset" } else { "https://github.com/$Repo/releases/download/$Version/$Asset" }
    $archive = Join-Path $staging $Asset
    try {
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
        Invoke-WebRequest -Uri $url -OutFile $archive -UseBasicParsing
    } catch {
        if (Get-Command gh -ErrorAction SilentlyContinue) {
            $tag = @(); if ($Version -ne 'latest') { $tag = @($Version) }
            & gh release download @tag --repo $Repo --pattern $Asset --dir $staging --clobber
        }
    }
    if (-not (Test-Path $archive) -or (Get-Item $archive).Length -eq 0) {
        throw "Could not download $url. Download $Asset from https://github.com/$Repo/releases, unpack it, and run install.ps1 from there."
    }
    Expand-Archive -Force (Join-Path $staging $Asset) $staging
    $source = (Get-ChildItem -Recurse $staging -Filter 'omega.exe' | Select-Object -First 1).FullName
    if (-not $source) { throw "$Asset holds no omega.exe" }
}

New-Item -ItemType Directory -Force $InstallDir | Out-Null
if (-not (Test-SameDirectory (Split-Path $source) $InstallDir)) {
    # A running MCP server keeps the old binary open. Windows lets an open file
    # be renamed but neither overwritten nor deleted, so the old one steps
    # aside -- under a name of its own: the one that stepped aside at the last
    # update may still be open in a session that has been running since.
    if (Test-Path $Exe) {
        Rename-Item $Exe "$(Split-Path -Leaf $Exe).old-$([DateTime]::UtcNow.Ticks)"
    }
    Copy-Item $source $Exe
    # Whatever no server holds any more goes now; the rest at a later update.
    Get-ChildItem -Path $InstallDir -Filter 'omega.exe.old*' | ForEach-Object {
        Remove-Item -Force $_.FullName -ErrorAction SilentlyContinue -Confirm:$false
    }
}
Write-Host "  binary       $Exe"
} finally {
    if ($staging) { Remove-Item -Recurse -Force $staging -ErrorAction SilentlyContinue -Confirm:$false }
    # And what earlier failed installs left behind.
    Get-ChildItem -Path ([IO.Path]::GetTempPath()) -Directory -Filter 'omega-install-*' -ErrorAction SilentlyContinue |
        Where-Object { $_.LastWriteTime -lt (Get-Date).AddHours(-1) } |
        ForEach-Object { Remove-Item -Recurse -Force $_.FullName -ErrorAction SilentlyContinue -Confirm:$false }
}

if (-not $NoPath) { Add-ToUserPath $InstallDir }

if (-not $NoModel) {
    Write-Host "  model        installing (32 MB, once)"
    & $Exe model install | Out-Null
    if ($LASTEXITCODE -ne 0) { Write-Warning "The model did not install; search stays lexical until 'omega model install' succeeds." }
}

Write-Host "`n  Done. Next: connect it to your coding agents with`n`n      omega install`n"
