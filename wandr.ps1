# wandr — desktop app manager for the wandr runtime (Windows).
#
#   wandr list                 apps in the registry  (o = installed)
#   wandr install <id> [ver]   download + install an app from the registry
#   wandr run <id>             run an installed app
#   wandr remove <id> [ver]    uninstall an app (all versions, or one)
#   wandr installed            list installed apps
#   wandr help
#
# Env: $env:WANDR_HOME (default %LOCALAPPDATA%\wandr),
#      $env:WANDR_REGISTRY (index.json URL or path).
# Install the runtime host with install.ps1 first.
[CmdletBinding()]
param([Parameter(Position=0)][string]$Command = 'help',
      [Parameter(ValueFromRemainingArguments=$true)][string[]]$Rest)

$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$WandrHome = if ($env:WANDR_HOME) { $env:WANDR_HOME } else { Join-Path $env:LOCALAPPDATA 'wandr' }
$BinDir    = Join-Path $WandrHome 'bin'
$HostExe   = Join-Path $BinDir 'wandr-host.exe'
# Host reads/writes installed apps at <WANDR_APPS_ROOT>\apps\<id>\<version>.
$AppsRoot     = $WandrHome
$InstalledDir = Join-Path $AppsRoot 'apps'
$Registry  = if ($env:WANDR_REGISTRY) { $env:WANDR_REGISTRY } else { 'https://harryzz.github.io/wandr/registry/index.json' }

function Info($m) { Write-Host "> $m" -ForegroundColor Green }
function Warn($m) { Write-Host "! $m" -ForegroundColor Yellow }
function Die($m)  { Write-Host "x $m" -ForegroundColor Red; exit 1 }
function Need-Host { if (-not (Test-Path $HostExe)) { Die "wandr-host not found at $HostExe - run install.ps1 first." } }

function Is-Local($u) { $u -like 'file://*' -or $u -match '^[A-Za-z]:\\' -or $u -like '.\*' -or $u -like '/*' -or (Test-Path -LiteralPath $u -ErrorAction SilentlyContinue) }
function Local-Path($u) {
  $p = if ($u -like 'file://*') { $u.Substring(7) } else { $u }
  if ($p -match '^/[A-Za-z]:') { $p = $p.Substring(1) }   # file:///C:/x -> C:/x
  $p
}

function Fetch-Text($u) {
  if (Is-Local $u) { Get-Content -Raw -LiteralPath (Local-Path $u) }
  else { (Invoke-WebRequest -Uri $u -UseBasicParsing).Content }
}
function Download($u, $out) {
  if (Is-Local $u) { Copy-Item -LiteralPath (Local-Path $u) -Destination $out -Force }
  else { Invoke-WebRequest -Uri $u -OutFile $out -UseBasicParsing }
}
function Registry-Apps {
  try { (Fetch-Text $Registry | ConvertFrom-Json).apps }
  catch { Die "cannot read registry: $Registry" }
}

function Cmd-List {
  if ($Rest -and ($Rest[0] -eq '--installed' -or $Rest[0] -eq '-i')) { Cmd-Installed; return }
  Write-Host "registry: $Registry"
  foreach ($a in Registry-Apps) {
    $mark = if (Test-Path (Join-Path $InstalledDir $a.id)) { 'o' } else { ' ' }
    $ver = if ($a.version) { $a.version } else { '?' }
    $nm  = if ($a.name)    { $a.name }    else { '' }
    '  {0} {1,-28} {2,-8} {3}' -f $mark, $a.id, $ver, $nm | Write-Host
  }
  Write-Host "`no = installed. Install:  wandr install <id>"
}

function Cmd-Installed {
  if (-not (Test-Path $InstalledDir)) { Info 'no apps installed.'; return }
  $found = $false
  foreach ($iddir in Get-ChildItem -Directory $InstalledDir) {
    foreach ($verdir in Get-ChildItem -Directory $iddir.FullName) {
      if (Test-Path (Join-Path $verdir.FullName 'package.toml')) {
        '  {0,-28} {1}' -f $iddir.Name, $verdir.Name | Write-Host; $found = $true
      }
    }
  }
  if (-not $found) { Info 'no apps installed.' }
}

function Cmd-Install {
  if (-not $Rest -or $Rest.Count -lt 1) { Die 'usage: wandr install <id> [version]' }
  Need-Host
  $id = $Rest[0]; $wantVer = if ($Rest.Count -ge 2) { $Rest[1] } else { $null }
  $app = Registry-Apps | Where-Object { $_.id -eq $id } | Select-Object -First 1
  if (-not $app) { Die "'$id' not found in the registry ($Registry)." }
  if ($wantVer -and $wantVer -ne $app.version) { Warn "registry has $id v$($app.version) (requested v$wantVer) - installing v$($app.version)." }

  $tmp = Join-Path $env:TEMP ("wandrpkg-" + [guid]::NewGuid().ToString() + ".wandrpkg")
  try {
    Info "downloading $id v$($app.version) ..."
    Download $app.url $tmp
    if ($app.sha256) {
      $got = (Get-FileHash -Algorithm SHA256 -LiteralPath $tmp).Hash.ToLower()
      if ($got -ne ("$($app.sha256)").ToLower()) { Die "checksum mismatch for $id." }
      Info 'checksum ok.'
    }
    Info 'installing ...'
    $env:WANDR_APPS_ROOT = $AppsRoot
    & $HostExe --install $tmp
    if ($LASTEXITCODE -ne 0) { Die 'install failed.' }
  } finally { Remove-Item -LiteralPath $tmp -ErrorAction SilentlyContinue }
  Info "installed $id v$($app.version) -> $InstalledDir\$id\$($app.version)"
  Write-Host "run it with:  wandr run $id"
}

function Cmd-Run {
  if (-not $Rest -or $Rest.Count -lt 1) { Die 'usage: wandr run <id>' }
  Need-Host
  $id = $Rest[0]
  if (-not (Test-Path (Join-Path $InstalledDir $id))) { Die "$id is not installed - try: wandr install $id" }
  $env:WANDR_APPS_ROOT = $AppsRoot
  & $HostExe --app $id
  exit $LASTEXITCODE
}

function Cmd-Remove {
  if (-not $Rest -or $Rest.Count -lt 1) { Die 'usage: wandr remove <id> [version]' }
  $id = $Rest[0]; $ver = if ($Rest.Count -ge 2) { $Rest[1] } else { $null }
  $target = if ($ver) { Join-Path (Join-Path $InstalledDir $id) $ver } else { Join-Path $InstalledDir $id }
  if (-not (Test-Path $target)) { Die "$id$(if($ver){" v$ver"}) is not installed." }
  Remove-Item -Recurse -Force $target
  if ($ver) { $idDir = Join-Path $InstalledDir $id; if ((Test-Path $idDir) -and -not (Get-ChildItem $idDir)) { Remove-Item -Force $idDir } }
  Info "removed $id$(if($ver){" v$ver"})."
}

function Cmd-Help {
@'
wandr - desktop app manager for the wandr runtime.

  wandr list                 apps in the registry  (o = installed)
  wandr list --installed     installed apps only
  wandr install <id> [ver]   download + install an app from the registry
  wandr run <id>             run an installed app
  wandr remove <id> [ver]    uninstall an app (all versions, or one)
  wandr installed            list installed apps
  wandr help

Env: WANDR_HOME (default %LOCALAPPDATA%\wandr), WANDR_REGISTRY (index.json URL or path)
'@ | Write-Host
}

switch ($Command.ToLower()) {
  'list'                     { Cmd-List }
  { $_ -in 'install','add' } { Cmd-Install }
  { $_ -in 'run','launch' }  { Cmd-Run }
  { $_ -in 'remove','rm','uninstall' } { Cmd-Remove }
  { $_ -in 'installed','ls' } { Cmd-Installed }
  { $_ -in 'help','-h','--help' } { Cmd-Help }
  default { Warn "unknown command: $Command"; Cmd-Help; exit 1 }
}
