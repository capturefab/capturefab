# Run only in an isolated test home. Restore the runner's user PATH even
# when an assertion fails; preserve unexpanded registry values and their kind.
$ErrorActionPreference = 'Stop'
$root = $env:CAPTUREFAB_INSTALL_FIXTURE
if (-not $root) { throw 'CAPTUREFAB_INSTALL_FIXTURE is required' }
$base = Join-Path $root 'apps [fixture] with spaces'
$key = Get-Item 'HKCU:\Environment'
$savedPath = $key.GetValue('Path', $null, 'DoNotExpandEnvironmentNames')
$savedKind = if ($null -ne $savedPath) { $key.GetValueKind('Path') } else { $null }
function Assert($condition, $message) { if (-not $condition) { throw $message } }
function RawPath { (Get-Item 'HKCU:\Environment').GetValue('Path', '', 'DoNotExpandEnvironmentNames') }
try {
  $env:LOCALAPPDATA = Join-Path $root 'local'
  $old = Join-Path $root 'old.ps1'
  $new = Join-Path $root 'new.ps1'
  $tools = Join-Path $base 'tools'
  New-Item -ItemType Directory -Path $tools -Force | Out-Null
  Set-Content -LiteralPath (Join-Path $base 'keep.txt') -Value 'unrelated'
  $foreign = Join-Path $base '8.8.8'
  New-Item -ItemType Directory -Path $foreign | Out-Null
  Set-Content -LiteralPath (Join-Path $foreign 'capturefab.exe') -Value 'another program'
  Set-Content -LiteralPath (Join-Path $foreign 'README.txt') -Value 'Other software'
  $extra = '%SystemRoot%\unrelated-fixture'
  New-ItemProperty 'HKCU:\Environment' -Name Path -PropertyType ExpandString -Value "$savedPath;$tools;$extra" -Force | Out-Null
  & $old -Prefix $base
  $first = Join-Path $base '9.9.8'
  $second = Join-Path $base '9.9.9'
  Assert (Test-Path -LiteralPath (Join-Path $first 'capturefab.exe')) 'initial install failed'
  # Existing 0.6.1 installs have a package README but no ownership marker.
  Remove-Item -LiteralPath (Join-Path $first '.capturefab-install')
  & $new -Prefix $base -Upgrade
  & $new -Prefix $base -Upgrade
  $entries = @((RawPath) -split ';')
  Assert ($entries -notcontains $first) 'upgrade kept the old PATH entry'
  Assert (@($entries | Where-Object { $_ -eq $second }).Count -eq 1) 'upgrade duplicated PATH'
  Assert ($entries -contains $tools -and $entries -contains $extra) 'upgrade changed unrelated PATH entries'
  Assert (($env:Path -split ';') -notcontains $first) 'upgrade kept the old process PATH entry'
  $before = RawPath
  $hash = (Get-FileHash -LiteralPath (Join-Path $second 'capturefab.exe')).Hash
  $corrupt = Join-Path $root 'corrupt.ps1'
  $script = (Get-Content -LiteralPath $new -Raw) -replace "Sha256 = '[0-9a-f]{64}'", ("Sha256 = '" + ('0' * 64) + "'")
  Set-Content -LiteralPath $corrupt -Value $script -Encoding UTF8
  $failed = $false
  try { & $corrupt -Prefix $base -Upgrade } catch { $failed = $true }
  Assert $failed 'corrupt upgrade succeeded'
  $failed = $false
  try { & (Join-Path $root 'broken.ps1') -Prefix $base -Upgrade } catch { $failed = $true }
  Assert $failed 'non-executable upgrade succeeded'
  Assert ((RawPath) -ceq $before) 'failed upgrade changed PATH'
  Assert ((Get-FileHash -LiteralPath (Join-Path $second 'capturefab.exe')).Hash -eq $hash) 'failed upgrade changed the executable'
  $env:CAPTUREFAB_INSTALL_BASE_URL = 'http://127.0.0.1:1'
  & $new -Prefix $base -Uninstall -DryRun
  Assert (Test-Path -LiteralPath (Join-Path $second 'capturefab.exe')) 'dry-run removed the executable'
  Assert ((RawPath) -ceq $before) 'dry-run changed PATH'
  $locked = [IO.File]::Open((Join-Path $second 'capturefab.exe'), 'Open', 'Read', 'Read')
  $failed = $false
  try { & $new -Prefix $base -Uninstall } catch { $failed = $true } finally { $locked.Dispose() }
  Assert $failed 'uninstall did not report a locked executable'
  Assert ((RawPath) -ceq $before) 'failed uninstall changed PATH'
  & $new -Prefix $base -Uninstall
  Assert (-not (Test-Path -LiteralPath $first) -and -not (Test-Path -LiteralPath $second)) 'uninstall left a release'
  Assert (Test-Path -LiteralPath $foreign) 'uninstall removed an unrecognized directory'
  Assert (Test-Path -LiteralPath (Join-Path $base 'keep.txt')) 'uninstall removed unrelated files'
  Assert ((RawPath) -split ';' -notcontains $second) 'uninstall kept its PATH entry'
  Assert ((RawPath) -split ';' -contains $tools) 'uninstall removed an unrelated PATH entry'
  & $new -Prefix $base -Uninstall
  Assert ((RawPath) -split ';' -contains $extra) 'repeat uninstall changed PATH'
} finally {
  if ($null -eq $savedPath) { Remove-ItemProperty 'HKCU:\Environment' -Name Path -ErrorAction SilentlyContinue }
  else { (Get-Item 'HKCU:\Environment').SetValue('Path', $savedPath, $savedKind) }
}
