<#
.SYNOPSIS
Install the `rapid` binary from a GitHub Release (Windows, x86_64).

.DESCRIPTION
  irm https://raw.githubusercontent.com/moss101/Rapidlm_CLI/main/scripts/install.ps1 | iex

Does what scripts/install.sh does, in the same order, and nothing else: download
the release's archive and its .sha256, refuse to go on unless the archive's
SHA-256 is the published one, unpack it in a temporary directory, check it holds
a regular rapid.exe, install it atomically (a copy beside the old file, then a
rename), run `rapid --version` once. It needs no administrator rights, edits no
registry key unless -AddToPath is given, and has no switch to skip the checksum.

.PARAMETER Version
A release tag such as v0.1.0. Default: the latest release.

.PARAMETER Dir
Where to put rapid.exe. Default: $env:RAPIDLM_INSTALL_DIR, else
$env:LOCALAPPDATA\Programs\rapidlm.

.PARAMETER DryRun
Say what would be downloaded and installed; change nothing.

.PARAMETER AddToPath
Add -Dir to the user's PATH (user scope, not machine scope).

Environment: RAPIDLM_REPO (owner/name, default moss101/Rapidlm_CLI) and
RAPIDLM_RELEASE_BASE (a base URL, or a directory path, holding the assets).
#>
[CmdletBinding()]
param(
    [string]$Version = 'latest',
    [string]$Dir = '',
    [switch]$DryRun,
    [switch]$AddToPath
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Fail([string]$Message) {
    Write-Error "install: $Message"
    exit 1
}

# A tag and a repository become part of a URL: keep them to what they look like.
if ($Version -ne 'latest' -and $Version -notmatch '^[A-Za-z0-9._-]+$') {
    Fail "the version '$Version' is not a release tag"
}
$Repo = if ($env:RAPIDLM_REPO) { $env:RAPIDLM_REPO } else { 'moss101/Rapidlm_CLI' }
if ($Repo -notmatch '^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$') {
    Fail 'RAPIDLM_REPO must be owner/name'
}

if (-not [Environment]::Is64BitOperatingSystem) {
    Fail 'there is no prebuilt binary for 32-bit Windows; build from source with: cargo install --path apps/rapid --locked'
}
# ARM64 Windows runs the x86_64 binary under emulation.
$Target = 'x86_64-pc-windows-msvc'
$Asset = "rapid-$Target.zip"

if ($env:RAPIDLM_RELEASE_BASE) {
    $Base = $env:RAPIDLM_RELEASE_BASE.TrimEnd('/', '\')
} elseif ($Version -eq 'latest') {
    $Base = "https://github.com/$Repo/releases/latest/download"
} else {
    $Base = "https://github.com/$Repo/releases/download/$Version"
}

if (-not $Dir) {
    $Dir = if ($env:RAPIDLM_INSTALL_DIR) { $env:RAPIDLM_INSTALL_DIR }
           else { Join-Path $env:LOCALAPPDATA 'Programs\rapidlm' }
}

Write-Host "rapid: $Version for $Target"
Write-Host "  from  $Base/$Asset"
Write-Host "  to    $(Join-Path $Dir 'rapid.exe')"
if ($DryRun) {
    Write-Host '(dry run: nothing was downloaded or installed)'
    return
}

[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$Work = Join-Path ([IO.Path]::GetTempPath()) ("rapid-install-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $Work | Out-Null
try {
    function Get-Asset([string]$Name, [string]$Out) {
        $source = "$Base/$Name"
        if ($Base -match '^https://') {
            Invoke-WebRequest -Uri $source -OutFile $Out -UseBasicParsing
        } elseif ($Base -match '^file://') {
            Copy-Item -LiteralPath ([Uri]$source).LocalPath -Destination $Out
        } elseif ($Base -match '^[A-Za-z]:\\' -or $Base.StartsWith('\\')) {
            Copy-Item -LiteralPath (Join-Path $Base $Name) -Destination $Out
        } else {
            Fail "RAPIDLM_RELEASE_BASE must be an https:// URL, a file:// URL or a directory"
        }
    }

    $Archive = Join-Path $Work $Asset
    $Sum = Join-Path $Work "$Asset.sha256"
    try { Get-Asset $Asset $Archive } catch { Fail "could not download $Base/$Asset ($($_.Exception.Message))" }
    try { Get-Asset "$Asset.sha256" $Sum } catch { Fail "could not download $Base/$Asset.sha256 ($($_.Exception.Message))" }

    $line = (Get-Content -LiteralPath $Sum -TotalCount 1).Trim()
    $parts = $line -split '\s+', 2
    $expected = $parts[0].ToLowerInvariant()
    if ($expected -notmatch '^[0-9a-f]{64}$') { Fail 'the published checksum file does not hold a SHA-256' }
    if ($parts.Count -gt 1) {
        $named = $parts[1].TrimStart('*').Trim()
        if ($named -ne $Asset) { Fail "the checksum file is for '$named', not '$Asset'" }
    }
    $actual = (Get-FileHash -LiteralPath $Archive -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $expected) {
        Fail "checksum mismatch for ${Asset}: expected $expected, got $actual. Nothing was installed."
    }
    Write-Host "  checksum ok  $actual"

    $Unpacked = Join-Path $Work 'unpacked'
    Expand-Archive -LiteralPath $Archive -DestinationPath $Unpacked
    $Exe = Join-Path $Unpacked 'rapid.exe'
    if (-not (Test-Path -LiteralPath $Exe -PathType Leaf)) { Fail 'the archive does not hold rapid.exe' }
    $item = Get-Item -LiteralPath $Exe
    if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { Fail 'the archive''s rapid.exe is a link, not a file' }
    if ($item.Length -eq 0) { Fail 'the archive''s rapid.exe is empty' }

    New-Item -ItemType Directory -Force -Path $Dir | Out-Null
    $Staged = Join-Path $Dir (".rapid.install." + [Guid]::NewGuid().ToString('N') + '.exe')
    try {
        Copy-Item -LiteralPath $Exe -Destination $Staged
        Move-Item -LiteralPath $Staged -Destination (Join-Path $Dir 'rapid.exe') -Force
    } catch {
        if (Test-Path -LiteralPath $Staged) { Remove-Item -LiteralPath $Staged -Force }
        Fail "could not move rapid.exe into place in ${Dir}: $($_.Exception.Message)"
    }

    $installed = & (Join-Path $Dir 'rapid.exe') --version 2>&1 | Select-Object -First 1
    Write-Host "  installed    $installed"

    $onPath = ($env:PATH -split ';') -contains $Dir
    if ($AddToPath) {
        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        if (-not (($userPath -split ';') -contains $Dir)) {
            [Environment]::SetEnvironmentVariable('Path', ($userPath.TrimEnd(';') + ';' + $Dir), 'User')
            Write-Host "  added $Dir to your user PATH; open a new terminal to use it"
        }
    } elseif (-not $onPath) {
        Write-Host ''
        Write-Host "$Dir is not on your PATH. Run again with -AddToPath, or add it yourself."
    }
} finally {
    Remove-Item -LiteralPath $Work -Recurse -Force -ErrorAction SilentlyContinue
}
