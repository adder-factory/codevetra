$ErrorActionPreference = 'Stop'

# Evaluate the real installer's environment-selection expressions without
# executing its platform checks, network, installation or persistent PATH writes.
$Installer = Join-Path $PSScriptRoot '../install.ps1'
$Tokens = $null
$ParseErrors = $null
$Ast = [System.Management.Automation.Language.Parser]::ParseFile($Installer, [ref]$Tokens, [ref]$ParseErrors)
if ($ParseErrors.Count -ne 0) { throw 'Windows installer does not parse' }
$Variables = @{
  repo = 'REPO'
  installDir = 'INSTALL_DIR'
  version = 'VERSION'
  skipChecksum = 'SKIP_CHECKSUM'
}
$Expressions = @{}
foreach ($Variable in $Variables.Keys) {
  $Assignment = $Ast.FindAll({
    param($Node)
    $Node -is [System.Management.Automation.Language.AssignmentStatementAst] -and
      $Node.Left -is [System.Management.Automation.Language.VariableExpressionAst] -and
      $Node.Left.VariablePath.UserPath -eq $Variable
  }, $true) | Select-Object -First 1
  if ($null -eq $Assignment) { throw "Missing installer assignment: $Variable" }
  $Expressions[$Variable] = [scriptblock]::Create($Assignment.Right.Extent.Text)
}

$Names = @($Variables.Values | ForEach-Object { "CODEVETRA_$_"; "CARTOGRAPH_$_" })
$Saved = @{}
foreach ($Name in $Names) { $Saved[$Name] = [Environment]::GetEnvironmentVariable($Name, 'Process') }

function Assert-Selections {
  param([hashtable]$Values, [hashtable]$Expected)
  foreach ($Name in $Names) { Remove-Item "Env:$Name" -ErrorAction SilentlyContinue }
  foreach ($Entry in $Values.GetEnumerator()) { [Environment]::SetEnvironmentVariable($Entry.Key, $Entry.Value, 'Process') }
  foreach ($Variable in $Variables.Keys) {
    $Actual = & $Expressions[$Variable]
    if ($Actual -cne $Expected[$Variable]) { throw "Unexpected installer precedence for ${Variable}: actual '$Actual', expected '$($Expected[$Variable])'" }
  }
}

try {
  $Legacy = @{
    CARTOGRAPH_REPO = 'legacy/repository'
    CARTOGRAPH_INSTALL_DIR = 'legacy-install'
    CARTOGRAPH_VERSION = '1.2.3'
    CARTOGRAPH_SKIP_CHECKSUM = '1'
  }
  Assert-Selections $Legacy @{ repo = 'legacy/repository'; installDir = 'legacy-install'; version = '1.2.3'; skipChecksum = '1' }
  $Canonical = $Legacy.Clone()
  $Canonical.CODEVETRA_REPO = 'canonical/repository'
  $Canonical.CODEVETRA_INSTALL_DIR = 'canonical-install'
  $Canonical.CODEVETRA_VERSION = '4.0.0'
  $Canonical.CODEVETRA_SKIP_CHECKSUM = '0'
  Assert-Selections $Canonical @{ repo = 'canonical/repository'; installDir = 'canonical-install'; version = '4.0.0'; skipChecksum = '0' }
  $Empty = $Legacy.Clone()
  foreach ($Suffix in $Variables.Values) { $Empty["CODEVETRA_$Suffix"] = '' }
  Assert-Selections $Empty @{ repo = ''; installDir = ''; version = ''; skipChecksum = '' }
  $Source = Get-Content -Raw $Installer
  $PlatformCheck = $Source.IndexOf('if (-not [Environment]::Is64BitOperatingSystem)')
  if ($PlatformCheck -lt 0) { throw 'Missing Windows platform boundary' }
  $Preamble = [scriptblock]::Create($Source.Substring(0, $PlatformCheck))
  foreach ($Name in @('CODEVETRA_REPO', 'CODEVETRA_INSTALL_DIR')) {
    foreach ($Entry in $Canonical.GetEnumerator()) { [Environment]::SetEnvironmentVariable($Entry.Key, $Entry.Value, 'Process') }
    [Environment]::SetEnvironmentVariable($Name, '', 'Process')
    $Refused = $false
    try { & $Preamble } catch {
      if ($_.Exception.Message -notlike '*must not be empty*') { throw }
      $Refused = $true
    }
    if (-not $Refused) { throw 'Empty canonical path reached the platform/network boundary' }
  }
  Write-Host 'Windows installer fallback, precedence, empty values and early refusal passed (12 selections, 2 refusals).'
} finally {
  foreach ($Name in $Names) {
    if ($null -eq $Saved[$Name]) { Remove-Item "Env:$Name" -ErrorAction SilentlyContinue }
    else { [Environment]::SetEnvironmentVariable($Name, $Saved[$Name], 'Process') }
  }
}
