$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'windows-rust-release-paths.ps1')

$WrapperNames = @(
  'RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER',
  'CARGO_BUILD_RUSTC_WRAPPER', 'CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER'
)

function Assert-ReleaseCargoArguments {
  param([string[]]$Actual, [string[]]$Expected)

  if ($Actual.Count -ne $Expected.Count) {
    throw 'release Cargo changed the number of command arguments'
  }
  for ($Index = 0; $Index -lt $Expected.Count; $Index++) {
    if ($Actual[$Index] -cne $Expected[$Index]) {
      throw 'release Cargo changed a command argument'
    }
  }
}

function Assert-ReleaseCompilerOutcome {
  param(
    [Parameter(Mandatory = $true)][string]$Outcome,
    [Parameter(Mandatory = $true)][hashtable]$ExpectedWrappers
  )

  $Failure = $null
  try {
    Invoke-WindowsReleaseCargo -CargoArguments @('probe', '--sentinel')
  } catch {
    $Failure = $_.Exception.Message
  }
  $ExpectedFailure = switch ($Outcome) {
    'failure' { 'Windows release Cargo command failed (exit 17)' }
    'throw' { 'simulated compiler launch failure' }
    default { $null }
  }
  if ($Failure -cne $ExpectedFailure) {
    throw "release Cargo did not preserve the $Outcome outcome: $Failure"
  }
  foreach ($Name in $WrapperNames) {
    if ([Environment]::GetEnvironmentVariable($Name) -cne $ExpectedWrappers[$Name]) {
      throw 'release Cargo did not restore the caller wrapper environment'
    }
  }
}

function Test-ReleaseCompilerPolicy {
  $SavedEnvironment = @{}
  foreach ($Name in $WrapperNames) {
    $SavedEnvironment[$Name] = [Environment]::GetEnvironmentVariable($Name)
  }
  $SavedExitCode = $global:LASTEXITCODE
  $InitialStates = @('inherited', 'empty', 'unset')
  $Outcomes = @('success', 'failure', 'throw')
  $ExpectedArguments = @(
    '--config', "build.rustc-wrapper=''",
    '--config', "build.rustc-workspace-wrapper=''", 'probe', '--sentinel'
  )
  $BuildEnvironment = @{}
  foreach ($Name in @('CARGO_ENCODED_RUSTFLAGS', 'RUSTFLAGS', 'CFLAGS', 'CXXFLAGS', 'CARGO_BUILD_JOBS', 'RUST_TEST_THREADS', 'CARGO_TARGET_DIR')) {
    $BuildEnvironment[$Name] = [Environment]::GetEnvironmentVariable($Name)
  }
  $Calls = [System.Collections.Generic.List[string]]::new()

  function cargo {
    foreach ($Name in $WrapperNames) {
      if ($null -ne [Environment]::GetEnvironmentVariable($Name)) {
        throw 'release Cargo inherited a compiler wrapper'
      }
    }
    Assert-ReleaseCargoArguments -Actual $args -Expected $ExpectedArguments
    foreach ($Name in $BuildEnvironment.Keys) {
      if ([Environment]::GetEnvironmentVariable($Name) -cne $BuildEnvironment[$Name]) {
        throw 'release Cargo changed compiler flags or build limits'
      }
    }
    $Calls.Add($Outcome)
    if ($Outcome -eq 'throw') { throw 'simulated compiler launch failure' }
    $global:LASTEXITCODE = if ($Outcome -eq 'failure') { 17 } else { 0 }
  }

  try {
    foreach ($InitialState in $InitialStates) {
      $ExpectedWrappers = @{}
      foreach ($Name in $WrapperNames) {
        if (Test-Path -LiteralPath "Env:$Name") { Remove-Item -LiteralPath "Env:$Name" }
        if ($InitialState -ne 'unset') {
          $Value = if ($InitialState -eq 'empty') { '' } else { "inherited-$Name" }
          [Environment]::SetEnvironmentVariable($Name, $Value)
        }
        $ExpectedWrappers[$Name] = [Environment]::GetEnvironmentVariable($Name)
      }
      foreach ($Outcome in $Outcomes) {
        Assert-ReleaseCompilerOutcome -Outcome $Outcome -ExpectedWrappers $ExpectedWrappers
      }
    }
    $ExpectedCalls = foreach ($InitialState in $InitialStates) { $Outcomes }
    if (($Calls -join ',') -cne ($ExpectedCalls -join ',')) {
      throw 'release Cargo did not execute each policy probe exactly once'
    }
  } finally {
    foreach ($Name in $SavedEnvironment.Keys) {
      if ($null -eq $SavedEnvironment[$Name]) {
        if (Test-Path -LiteralPath "Env:$Name") { Remove-Item -LiteralPath "Env:$Name" }
      } else {
        [Environment]::SetEnvironmentVariable($Name, $SavedEnvironment[$Name])
      }
    }
    $global:LASTEXITCODE = $SavedExitCode
  }
}

function New-ReleaseCompilerProbe {
  param([Parameter(Mandatory = $true)][string]$Directory)

  $Utf8 = [System.Text.UTF8Encoding]::new($false)
  [System.IO.Directory]::CreateDirectory((Join-Path $Directory 'src')) | Out-Null
  [System.IO.Directory]::CreateDirectory((Join-Path $Directory '.cargo')) | Out-Null
  $Manifest = @'
[package]
name = "release_argument_probe"
version = "0.0.0"
edition = "2024"
[workspace]
'@
  $Lockfile = "version = 4`n[[package]]`nname = `"release_argument_probe`"`nversion = `"0.0.0`"`n"
  $Library = @'
#[cfg(not(release_probe_tail))]
compile_error!("compiler argument tail was dropped");
pub fn value() -> &'static str { "response-file-complete" }
'@
  $Source = 'fn main() { println!("{}", release_argument_probe::value()); }'
  [System.IO.File]::WriteAllText((Join-Path $Directory 'Cargo.toml'), $Manifest, $Utf8)
  [System.IO.File]::WriteAllText((Join-Path $Directory 'Cargo.lock'), $Lockfile, $Utf8)
  [System.IO.File]::WriteAllText((Join-Path $Directory 'src/main.rs'), $Source, $Utf8)
  [System.IO.File]::WriteAllText((Join-Path $Directory 'src/lib.rs'), $Library, $Utf8)
  Copy-Item (Join-Path $PSScriptRoot '../rust-toolchain.toml') $Directory

  # CreateProcessW includes the terminating null in its 32,767-character limit.
  $WindowsCommandLineLimit = 32767
  $Flags = [System.Collections.Generic.List[string]]::new()
  $CommandLength = 0
  for ($Index = 0; $CommandLength -le $WindowsCommandLineLimit; $Index++) {
    $Flag = "--cfg=release_argument_padding_$Index"
    $Flags.Add($Flag)
    $CommandLength += $Flag.Length + 1
  }
  $Flags.Add('--check-cfg=cfg(release_probe_tail)')
  $Flags.Add('--cfg=release_probe_tail')
  if (($Flags -join ' ').Length -le $WindowsCommandLineLimit) {
    throw 'compiler probe does not exceed the Windows command-line limit'
  }
  $EncodedFlags = ConvertTo-Json -InputObject $Flags.ToArray() -Compress
  $Config = "[build]`njobs = 2`nrustc-wrapper = 'sccache'`nrustc-workspace-wrapper = 'sccache'`nrustflags = $EncodedFlags`n"
  [System.IO.File]::WriteAllText((Join-Path $Directory '.cargo/config.toml'), $Config, $Utf8)
}

function Assert-WrappedCompilerOverflow {
  param([Parameter(Mandatory = $true)][string]$Directory)

  foreach ($Name in @('RUSTC_WORKSPACE_WRAPPER', 'CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER')) {
    if (Test-Path -LiteralPath "Env:$Name") { Remove-Item -LiteralPath "Env:$Name" }
  }
  $Log = Join-Path $Directory 'wrapped-compiler.log'
  # This negative control inspects the exact native failure below. Keep the
  # preference local so a caller's native-command error policy is restored.
  $PSNativeCommandUseErrorActionPreference = $false
  & cargo --config "build.rustc-workspace-wrapper=''" build --offline --locked --release *> $Log
  $WrappedExitCode = $LASTEXITCODE
  if ($WrappedExitCode -eq 0 -or (Get-Item -LiteralPath $Log).Length -gt 2MB -or
      -not [System.IO.File]::ReadAllText($Log).Contains('os error 206')) {
    throw 'the pinned compiler-cache negative control did not reproduce Windows command-line overflow'
  }
  foreach ($Name in $WrapperNames) {
    [Environment]::SetEnvironmentVariable($Name, 'sccache')
  }
}

function Test-ReleaseCompilerArguments {
  $Directory = Join-Path ([System.IO.Path]::GetTempPath()) "codevetra-compiler-probe-$([guid]::NewGuid())"
  $SavedEnvironment = @{}
  foreach ($Name in @($WrapperNames) + @('RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'CARGO_TARGET_DIR', 'CARGO_BUILD_BUILD_DIR')) {
    $SavedEnvironment[$Name] = [Environment]::GetEnvironmentVariable($Name)
  }
  try {
    New-ReleaseCompilerProbe -Directory $Directory
    foreach ($Name in $WrapperNames) {
      [Environment]::SetEnvironmentVariable($Name, 'sccache')
    }
    $env:RUSTFLAGS = $null
    $env:CARGO_ENCODED_RUSTFLAGS = $null
    $env:CARGO_TARGET_DIR = Join-Path $Directory 'target'
    $env:CARGO_BUILD_BUILD_DIR = $env:CARGO_TARGET_DIR
    Push-Location $Directory
    try {
      Assert-WrappedCompilerOverflow -Directory $Directory
      Invoke-WindowsReleaseCargo -CargoArguments @('build', '--offline', '--locked', '--release')
      $Output = & (Join-Path $env:CARGO_TARGET_DIR 'release/release_argument_probe.exe')
      if ($LASTEXITCODE -ne 0 -or $Output -cne 'response-file-complete') {
        throw 'release compiler argument probe did not execute successfully'
      }
    } finally {
      Pop-Location
    }
  } finally {
    foreach ($Name in $SavedEnvironment.Keys) {
      if ($null -eq $SavedEnvironment[$Name]) {
        if (Test-Path -LiteralPath "Env:$Name") { Remove-Item -LiteralPath "Env:$Name" }
      } else {
        [Environment]::SetEnvironmentVariable($Name, $SavedEnvironment[$Name])
      }
    }
    if ([System.IO.Directory]::Exists($Directory)) {
      [System.IO.Directory]::Delete($Directory, $true)
    }
  }
}

Test-ReleaseCompilerPolicy
if ($IsWindows) {
  Test-ReleaseCompilerArguments
  Write-Host '[rust-release] Windows compiler policy and oversized argument regression passed'
} else {
  Write-Host '[rust-release] compiler policy passed; native oversized argument probe requires Windows'
}
