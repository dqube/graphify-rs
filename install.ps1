<#
.SYNOPSIS
    graphify-rs installer for Windows.

.DESCRIPTION
    Downloads a prebuilt binary from GitHub Releases and puts it on your PATH.
    No Rust toolchain required.

.EXAMPLE
    irm https://raw.githubusercontent.com/dqube/graphify-rs/main/install.ps1 | iex

.EXAMPLE
    # Pin a version and choose the install directory
    $env:GRAPHIFY_VERSION = 'v0.8.2'
    $env:GRAPHIFY_INSTALL_DIR = 'C:\tools\graphify'
    irm https://raw.githubusercontent.com/dqube/graphify-rs/main/install.ps1 | iex
#>

[CmdletBinding()]
param(
    [string]$Version    = $env:GRAPHIFY_VERSION,
    [string]$InstallDir = $env:GRAPHIFY_INSTALL_DIR,
    [string]$Repo       = $(if ($env:GRAPHIFY_REPO) { $env:GRAPHIFY_REPO } else { 'dqube/graphify-rs' })
)

$ErrorActionPreference = 'Stop'
# TLS 1.2 is not the default on Windows PowerShell 5.1, and GitHub requires it.
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$Bin = 'graphify-rs'
if (-not $InstallDir) { $InstallDir = Join-Path $env:LOCALAPPDATA "$Bin\bin" }

function Write-Info { param($m) Write-Host "  $m" }
function Write-Warn { param($m) Write-Host "  warning: $m" -ForegroundColor Yellow }
function Die       { param($m) Write-Host "error: $m" -ForegroundColor Red; exit 1 }

# --- resolve target --------------------------------------------------------

# Only x64 binaries are published; Windows on ARM runs them under emulation,
# so map both to the same artifact rather than failing.
$arch = $env:PROCESSOR_ARCHITECTURE
switch ($arch) {
    'AMD64' { $target = 'x86_64-pc-windows-msvc' }
    'ARM64' { $target = 'x86_64-pc-windows-msvc'; Write-Warn 'No native ARM64 build yet; installing the x64 binary (runs under emulation).' }
    'x86'   { Die "32-bit Windows is not supported." }
    default { $target = 'x86_64-pc-windows-msvc' }
}

# --- resolve version -------------------------------------------------------

if (-not $Version) {
    try {
        $rel = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" `
                                 -Headers @{ 'User-Agent' = 'graphify-rs-installer' }
        $Version = $rel.tag_name
    } catch {
        Die "could not reach GitHub to find the latest release. Set `$env:GRAPHIFY_VERSION to install a specific tag."
    }
}
if (-not $Version) { Die "could not determine a version to install." }

$archive = "$Bin-$target.zip"
$base    = "https://github.com/$Repo/releases/download/$Version"

Write-Host ""
Write-Host "  Installing $Bin $Version ($target)"
Write-Host ""

# --- download --------------------------------------------------------------

$tmp = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid().ToString())
New-Item -ItemType Directory -Path $tmp -Force | Out-Null

try {
    Write-Info "Downloading $archive..."
    $zipPath = Join-Path $tmp $archive
    try {
        Invoke-WebRequest -Uri "$base/$archive" -OutFile $zipPath -UseBasicParsing
    } catch {
        Die "download failed. Does $Version ship a build for $target? See https://github.com/$Repo/releases"
    }

    # Verifying the archive matters here: the installer runs what it downloads.
    $sumsPath = Join-Path $tmp 'SHA256SUMS'
    try {
        Invoke-WebRequest -Uri "$base/SHA256SUMS" -OutFile $sumsPath -UseBasicParsing
        # Lines are "<hash>  <filename>". Split on whitespace and match the
        # filename field exactly rather than regex-matching the whole line.
        $expected = $null
        foreach ($line in Get-Content $sumsPath) {
            $parts = $line.Trim() -split '\s+'
            if ($parts.Count -ge 2 -and $parts[-1] -eq $archive) { $expected = $parts[0]; break }
        }
        if ($expected) {
            $actual = (Get-FileHash $zipPath -Algorithm SHA256).Hash.ToLower()
            if ($actual -ne $expected.ToLower()) {
                Die "checksum mismatch for $archive (expected $expected, got $actual)."
            }
            Write-Info "Checksum verified."
        } else {
            Write-Warn "no checksum published for $archive; skipping verification."
        }
    } catch {
        Write-Warn "SHA256SUMS not published for $Version; skipping checksum verification."
    }

    # --- install -----------------------------------------------------------

    Expand-Archive -Path $zipPath -DestinationPath $tmp -Force
    # Archives contain a <name>-<target>\ directory; tolerate a flat layout
    # too so older releases keep installing.
    $src = Join-Path $tmp "$Bin-$target\$Bin.exe"
    if (-not (Test-Path $src)) { $src = Join-Path $tmp "$Bin.exe" }
    if (-not (Test-Path $src)) { Die "archive did not contain $Bin.exe." }

    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    Copy-Item $src (Join-Path $InstallDir "$Bin.exe") -Force

    Write-Host ""
    Write-Info "Installed to $(Join-Path $InstallDir "$Bin.exe")"
}
finally {
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}

# --- PATH ------------------------------------------------------------------

# Persist to the user PATH, and also update this session so the command works
# immediately without a new terminal.
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if ($userPath -notlike "*$InstallDir*") {
    $newPath = if ([string]::IsNullOrEmpty($userPath)) { $InstallDir } else { "$userPath;$InstallDir" }
    [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
    Write-Info "Added $InstallDir to your user PATH."
}
if ($env:Path -notlike "*$InstallDir*") { $env:Path = "$env:Path;$InstallDir" }

Write-Host ""
Write-Info "Run: $Bin --help"
Write-Host ""
