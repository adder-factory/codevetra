$ErrorActionPreference = 'Stop'

$repo = if (Test-Path Env:CODEVETRA_REPO) { $env:CODEVETRA_REPO } elseif (Test-Path Env:CARTOGRAPH_REPO) { $env:CARTOGRAPH_REPO } else { 'adder-factory/codevetra' }
$installDir = if (Test-Path Env:CODEVETRA_INSTALL_DIR) { $env:CODEVETRA_INSTALL_DIR } elseif (Test-Path Env:CARTOGRAPH_INSTALL_DIR) { $env:CARTOGRAPH_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'codevetra-cli' }

if ([string]::IsNullOrEmpty($repo) -or [string]::IsNullOrEmpty($installDir)) {
  throw 'codevetra: repository and install directory must not be empty; unset an override to use its default.'
}

if (-not [Environment]::Is64BitOperatingSystem) {
  throw 'codevetra: 32-bit Windows is not supported; use a current 64-bit Windows release.'
}
$windowsVersion = [Environment]::OSVersion.Version
$windowsInstallationType = (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion').InstallationType
$windowsMinimumBuild = if ($windowsInstallationType -like '*Server*') { 26100 } else { 26200 }
$windowsMinimumRelease = if ($windowsInstallationType -like '*Server*') { 'Windows Server 2025' } else { 'Windows 11 25H2' }
if ($windowsVersion.Major -lt 10 -or $windowsVersion.Build -lt $windowsMinimumBuild) {
  throw "codevetra: $windowsMinimumRelease (build $windowsMinimumBuild) or newer is required; found $windowsVersion."
}

# Only 64-bit x64 builds are published; Windows-on-ARM runs them via the
# built-in x64 emulation.
$target = 'windows-x64'

$version = if (Test-Path Env:CODEVETRA_VERSION) { $env:CODEVETRA_VERSION } else { $env:CARTOGRAPH_VERSION }
if (-not $version) {
  $version = (Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest").tag_name
}
if (-not $version) { throw 'codevetra: could not resolve latest release; set CODEVETRA_VERSION.' }
if (-not $version.StartsWith('v')) { $version = "v$version" }

$url = "https://github.com/$repo/releases/download/$version/codevetra-$target.zip"
$tmp = Join-Path $env:TEMP ("codevetra-" + [guid]::NewGuid().ToString())
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
$zip = Join-Path $tmp 'codevetra.zip'

Write-Host "Installing CodeVetra $version ($target)..."
Invoke-WebRequest -Uri $url -OutFile $zip

# Verify the archive against the release's SHA256SUMS before extracting an
# executable. Set CODEVETRA_SKIP_CHECKSUM=1 to bypass (not recommended).
$skipChecksum = if (Test-Path Env:CODEVETRA_SKIP_CHECKSUM) { $env:CODEVETRA_SKIP_CHECKSUM } else { $env:CARTOGRAPH_SKIP_CHECKSUM }
if ($skipChecksum -ne '1') {
  $sumsUrl = "https://github.com/$repo/releases/download/$version/SHA256SUMS"
  $sumsPath = Join-Path $tmp 'SHA256SUMS'
  try {
    Invoke-WebRequest -Uri $sumsUrl -OutFile $sumsPath
  } catch {
    throw "codevetra: could not fetch SHA256SUMS for verification ($sumsUrl). Set CODEVETRA_SKIP_CHECKSUM=1 to install without verification."
  }
  $assetName = "codevetra-$target.zip"
  $expectedLine = Get-Content $sumsPath | Where-Object { $_ -match [regex]::Escape($assetName) } | Select-Object -First 1
  if (-not $expectedLine) { throw "codevetra: no checksum for $assetName in SHA256SUMS." }
  $expected = ($expectedLine -split '\s+')[0].ToLower()
  $actual = (Get-FileHash -Algorithm SHA256 -Path $zip).Hash.ToLower()
  if ($expected -ne $actual) {
    throw "codevetra: CHECKSUM MISMATCH - refusing to install.`n  expected $expected`n  actual   $actual"
  }
  Write-Host 'Verified checksum.'
}

$dest = Join-Path $installDir 'current'
if (Test-Path $dest) { Remove-Item -Recurse -Force $dest }
New-Item -ItemType Directory -Force -Path $dest | Out-Null
Expand-Archive -Path $zip -DestinationPath $dest -Force

$inner = Join-Path $dest "codevetra-$target"
if (Test-Path $inner) {
  Get-ChildItem -Force $inner | Move-Item -Destination $dest -Force
  Remove-Item -Recurse -Force $inner
}
Remove-Item -Recurse -Force $tmp

$binDir = Join-Path $dest 'bin'
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($userPath -split ';') -notcontains $binDir) {
  [Environment]::SetEnvironmentVariable('Path', "$binDir;$userPath", 'User')
  Write-Host "Added $binDir to your PATH. Restart your terminal to pick it up."
}

Write-Host "Installed to $dest"
Write-Host 'Run: codevetra --help'
Write-Host ''
Write-Host 'Native RocksDB project setup:'
Write-Host '  cd /path/to/your/project'
Write-Host '  codevetra index .'
Write-Host '  codevetra install --yes --target codex --location local'
Write-Host ''
Write-Host 'Use --target claude or --target cursor for those agent hosts.'
