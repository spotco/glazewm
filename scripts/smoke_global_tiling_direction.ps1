[CmdletBinding()]
param(
  [string]$CliPath,
  [string]$LogPath,
  [switch]$TestSoftExit,
  [switch]$SkipLiveMoves
)

$ErrorActionPreference = 'Stop'
$script:FailureCount = 0
$script:SnapshotPath = $null
$script:SnapshotReady = $false
$script:OriginalGlobalDirection = $null

$runId = Get-Date -Format 'yyyyMMdd-HHmmss'
if ([string]::IsNullOrWhiteSpace($LogPath)) {
  $LogPath = Join-Path $PSScriptRoot '..\plans\2026-09-26\GLOBAL_TILING_DIRECTION_SMOKE.log'
}
$LogPath = [IO.Path]::GetFullPath($LogPath)
$logDirectory = Split-Path -Parent $LogPath
if (-not (Test-Path -LiteralPath $logDirectory)) {
  New-Item -ItemType Directory -Path $logDirectory -Force | Out-Null
}
Set-Content -LiteralPath $LogPath -Value $null -Encoding utf8

function Write-SmokeLog {
  param(
    [Parameter(Mandatory)] [string]$Event,
    [hashtable]$Fields = @{}
  )

  $record = [ordered]@{
    timestamp = (Get-Date).ToUniversalTime().ToString('o')
    event = $Event
  }
  foreach ($key in $Fields.Keys) {
    $record[$key] = $Fields[$key]
  }
  $line = $record | ConvertTo-Json -Depth 20 -Compress
  Add-Content -LiteralPath $LogPath -Value $line -Encoding utf8
  Write-Host $line
}

function Resolve-GlazeWmCli {
  if (-not [string]::IsNullOrWhiteSpace($CliPath)) {
    if (-not (Test-Path -LiteralPath $CliPath)) {
      throw "CLI path does not exist: $CliPath"
    }
    return [IO.Path]::GetFullPath($CliPath)
  }

  $candidates = @(
    (Join-Path $env:ProgramFiles 'glzr.io\GlazeWM\glazewm-cli.exe'),
    (Join-Path $PSScriptRoot '..\target\release\glazewm-cli.exe')
  )
  foreach ($candidate in $candidates) {
    if (Test-Path -LiteralPath $candidate) {
      return [IO.Path]::GetFullPath($candidate)
    }
  }

  $command = Get-Command glazewm-cli.exe -ErrorAction SilentlyContinue
  if ($null -ne $command) {
    return $command.Source
  }
  throw 'Unable to locate glazewm-cli.exe.'
}

function Invoke-GlazeWmCli {
  param(
    [Parameter(Mandatory)] [string[]]$Arguments,
    [Parameter(Mandatory)] [string]$Name,
    [switch]$Json
  )

  Write-SmokeLog -Event 'cli_call' -Fields @{
    name = $Name
    arguments = $Arguments
  }
  $output = @(& $script:Cli @Arguments 2>&1 | ForEach-Object { $_.ToString() })
  $exitCode = $LASTEXITCODE
  $text = $output -join "`n"
  Write-SmokeLog -Event 'cli_result' -Fields @{
    name = $Name
    exitCode = $exitCode
    outputLength = $text.Length
  }
  if ($exitCode -ne 0) {
    throw "$Name failed with exit code ${exitCode}: $text"
  }
  if ($Json) {
    try {
      return $text | ConvertFrom-Json
    } catch {
      throw "$Name returned invalid JSON: $text"
    }
  }
  return $text
}

function Assert-Smoke {
  param(
    [Parameter(Mandatory)] [string]$Name,
    [Parameter(Mandatory)] [bool]$Condition,
    [hashtable]$Fields = @{}
  )

  $Fields['pass'] = $Condition
  Write-SmokeLog -Event 'assertion' -Fields (@{ name = $Name } + $Fields)
  if (-not $Condition) {
    $script:FailureCount += 1
  }
}

function Get-GlobalDirection {
  $response = Invoke-GlazeWmCli @('query', 'global-tiling-direction') 'query_global_direction' -Json
  return $response.data.globalTilingDirection
}

function Get-Layout {
  return Invoke-GlazeWmCli @('query', 'layout') 'query_layout' -Json
}

function Get-FocusedContainer {
  $response = Invoke-GlazeWmCli @('query', 'focused') 'query_focused' -Json
  return $response.data.focused
}

function Get-IgnoredWindows {
  $response = Invoke-GlazeWmCli @('query', 'ignored') 'query_ignored' -Json
  return @($response.data.windows)
}

function Convert-ToStructuralValue {
  param([AllowNull()]$Value)

  if ($null -eq $Value) {
    return $null
  }
  if (
    $Value -is [System.Collections.IEnumerable] -and
    $Value -isnot [string] -and
    $Value -isnot [pscustomobject]
  ) {
    return @($Value | ForEach-Object { Convert-ToStructuralValue $_ })
  }
  if ($Value -is [pscustomobject]) {
    $transient = @(
      'hasFocus',
      'childFocusOrder',
      'title',
      'titleHint',
      'activeDrag',
      'floatingPlacement',
      'prevState'
    )
    $result = [ordered]@{}
    foreach ($property in $Value.PSObject.Properties) {
      if ($transient -notcontains $property.Name) {
        $result[$property.Name] = Convert-ToStructuralValue $property.Value
      }
    }
    return [pscustomobject]$result
  }
  return $Value
}

function Get-StructuralLayoutDigest {
  param([Parameter(Mandatory)]$LayoutResponse)
  return (
    Convert-ToStructuralValue $LayoutResponse.data.monitors |
      ConvertTo-Json -Depth 100 -Compress
  )
}

function Get-Sha256 {
  param([Parameter(Mandatory)] [string]$Text)
  $sha = [Security.Cryptography.SHA256]::Create()
  try {
    $bytes = [Text.Encoding]::UTF8.GetBytes($Text)
    return (($sha.ComputeHash($bytes) | ForEach-Object { $_.ToString('x2') }) -join '')
  } finally {
    $sha.Dispose()
  }
}

function Get-StructuralLayoutHash {
  param([Parameter(Mandatory)]$LayoutResponse)
  return Get-Sha256 (Get-StructuralLayoutDigest $LayoutResponse)
}

function Write-LayoutLogTail {
  param([string]$Reason)

  $layoutLogPath = Join-Path $env:USERPROFILE '.glzr\glazewm\layout.log'
  $lines = @()
  if (Test-Path -LiteralPath $layoutLogPath) {
    $lines = @(Get-Content -LiteralPath $layoutLogPath -Tail 12)
  }
  Write-SmokeLog -Event 'layout_log_tail' -Fields @{
    reason = $Reason
    path = $layoutLogPath
    lines = ($lines -join "`n")
  }
}

function Wait-ForCondition {
  param(
    [Parameter(Mandatory)] [scriptblock]$Condition,
    [int]$TimeoutSeconds = 8
  )

  $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
  do {
    try {
      if (& $Condition) {
        return $true
      }
    } catch {
      # Retry until the timeout; this is expected while WM/IPC restarts.
    }
    Start-Sleep -Milliseconds 250
  } while ((Get-Date) -lt $deadline)
  return $false
}

function Start-SpotcoZebar {
  $zebarPath = Join-Path $env:ProgramFiles 'glzr.io\Zebar\zebar.exe'
  if (-not (Test-Path -LiteralPath $zebarPath)) {
    throw "Zebar executable does not exist: $zebarPath"
  }
  Get-Process zebar -ErrorAction SilentlyContinue |
    Stop-Process -Force -ErrorAction SilentlyContinue
  $process = Start-Process -FilePath $zebarPath -ArgumentList @(
    'start-widget-preset',
    '--pack', 'spotco.tokyo-silence',
    '--widget-name', 'bar',
    '--preset', 'default'
  ) -WindowStyle Hidden -PassThru
  Write-SmokeLog -Event 'zebar_start' -Fields @{ pid = $process.Id; pack = 'spotco.tokyo-silence' }
  return $process
}

$script:Cli = Resolve-GlazeWmCli
$script:SnapshotPath = Join-Path $env:TEMP "glazewm global direction smoke $runId.json"
Write-SmokeLog -Event 'run_start' -Fields @{
  runId = $runId
  cli = $script:Cli
  log = $LogPath
  snapshot = $script:SnapshotPath
  testSoftExit = [bool]$TestSoftExit
}

try {
  $script:OriginalGlobalDirection = Get-GlobalDirection
  $initialLayout = Get-Layout
  Write-SmokeLog -Event 'initial_state' -Fields @{
    globalDirection = $script:OriginalGlobalDirection
    structuralHash = (Get-StructuralLayoutHash $initialLayout)
  }

  $ignored = Get-IgnoredWindows
  $spotcoWindow = @($ignored | Where-Object {
      $_.identity.processName -eq 'zebar' -and
      $_.identity.titleHint -like '*spotco.tokyo-silence*'
    })
  $zebarProcess = @(Get-Process zebar -ErrorAction SilentlyContinue)
  Assert-Smoke 'spotco Zebar bar is running and WM-ignored' (
    $spotcoWindow.Count -gt 0 -and $zebarProcess.Count -gt 0
  ) -Fields @{
    ignoredMatches = $spotcoWindow.Count
    processCount = $zebarProcess.Count
  }

  $snapshotOutput = Invoke-GlazeWmCli @('save-layout', $script:SnapshotPath) 'save_smoke_snapshot'
  $script:SnapshotReady = Test-Path -LiteralPath $script:SnapshotPath
  Assert-Smoke 'layout snapshot created for safe restore' $script:SnapshotReady -Fields @{
    path = $script:SnapshotPath
    outputLength = $snapshotOutput.Length
  }

  $beforeToggleLayout = Get-Layout
  $beforeToggleDirection = Get-GlobalDirection
  Invoke-GlazeWmCli @('command', 'toggle-tiling-direction') 'toggle_global_direction' | Out-Null
  Start-Sleep -Milliseconds 250
  $afterToggleLayout = Get-Layout
  $afterToggleDirection = Get-GlobalDirection
  Assert-Smoke 'toggle changes global direction' (
    $afterToggleDirection -ne $beforeToggleDirection
  ) -Fields @{ before = $beforeToggleDirection; after = $afterToggleDirection }
  Assert-Smoke 'toggle does not change structural layout' (
    (Get-StructuralLayoutDigest $beforeToggleLayout) -eq
      (Get-StructuralLayoutDigest $afterToggleLayout)
  )
  Invoke-GlazeWmCli @('command', 'set-tiling-direction', $beforeToggleDirection) 'restore_global_direction_after_toggle' | Out-Null
  Assert-Smoke 'toggle restores original global direction' (
    (Get-GlobalDirection) -eq $beforeToggleDirection
  )
  Write-LayoutLogTail 'global toggle'

  if (-not $SkipLiveMoves) {
    $focused = Get-FocusedContainer
    if ($focused.state.type -ne 'tiling') {
      $windowsResponse = Invoke-GlazeWmCli @('query', 'windows') 'query_windows_for_tiling_focus' -Json
      $tilingWindow = @($windowsResponse.data.windows | Where-Object {
          $_.state.type -eq 'tiling'
        } | Select-Object -First 1)
      if ($tilingWindow.Count -gt 0) {
        Invoke-GlazeWmCli @(
          'command', 'focus', '--container-id', $tilingWindow[0].id
        ) 'focus_tiling_window_for_move_smoke' | Out-Null
        Start-Sleep -Milliseconds 250
        $focused = Get-FocusedContainer
      }
    }

    Assert-Smoke 'a tiling focus is available for live move smoke' (
      $focused.state.type -eq 'tiling'
    ) -Fields @{ focusedType = $focused.state.type; focusedId = $focused.id }

    if ($focused.state.type -eq 'tiling') {
      $moveCases = @(
        @{ name = 'global-horizontal-normal'; global = 'horizontal'; opposite = $false },
        @{ name = 'global-vertical-normal'; global = 'vertical'; opposite = $false },
        @{ name = 'global-horizontal-opposite'; global = 'horizontal'; opposite = $true }
      )
      foreach ($case in $moveCases) {
        Invoke-GlazeWmCli @(
          'command', 'set-tiling-direction', $case.global
        ) "set_global_for_$($case.name)" | Out-Null
        $beforeMove = Get-Layout
        $beforeMoveDirection = Get-GlobalDirection
        $moveArguments = @('command', 'move', '--direction', 'left')
        if ($case.opposite) {
          $moveArguments += '--opposite-tiling-direction'
        }
        Invoke-GlazeWmCli $moveArguments "move_$($case.name)" | Out-Null
        Start-Sleep -Milliseconds 350
        $afterMove = Get-Layout
        $afterMoveDirection = Get-GlobalDirection
        Assert-Smoke "$($case.name) keeps stored global direction" (
          $afterMoveDirection -eq $beforeMoveDirection
        ) -Fields @{
          global = $case.global
          opposite = [bool]$case.opposite
          beforeHash = (Get-StructuralLayoutHash $beforeMove)
          afterHash = (Get-StructuralLayoutHash $afterMove)
        }
        Write-LayoutLogTail $case.name
      }
    }
  }
} catch {
  $script:FailureCount += 1
  Write-SmokeLog -Event 'fatal_error' -Fields @{ message = $_.Exception.Message }
} finally {
  if ($script:SnapshotReady) {
    try {
      Invoke-GlazeWmCli @('load-layout', $script:SnapshotPath) 'restore_smoke_snapshot' | Out-Null
      Write-SmokeLog -Event 'restore_complete' -Fields @{ path = $script:SnapshotPath }
    } catch {
      $script:FailureCount += 1
      Write-SmokeLog -Event 'restore_failed' -Fields @{ message = $_.Exception.Message }
    }
  }
  if ($null -ne $script:OriginalGlobalDirection) {
    try {
      Invoke-GlazeWmCli @(
        'command', 'set-tiling-direction', $script:OriginalGlobalDirection
      ) 'restore_original_global_direction' | Out-Null
    } catch {
      $script:FailureCount += 1
      Write-SmokeLog -Event 'direction_restore_failed' -Fields @{ message = $_.Exception.Message }
    }
  }
  if ($null -ne $script:SnapshotPath -and (Test-Path -LiteralPath $script:SnapshotPath)) {
    Remove-Item -LiteralPath $script:SnapshotPath -Force -ErrorAction SilentlyContinue
  }
}

if ($TestSoftExit -and $script:FailureCount -eq 0) {
  try {
    $beforeExitZebar = @(Get-Process zebar -ErrorAction SilentlyContinue)
    Invoke-GlazeWmCli @('command', 'wm-exit') 'soft_wm_exit' | Out-Null
    $zebarStopped = Wait-ForCondition {
      @(Get-Process zebar -ErrorAction SilentlyContinue).Count -eq 0
    }
    Assert-Smoke 'soft wm-exit kills Zebar via shutdown_commands' $zebarStopped -Fields @{
      processCountBefore = $beforeExitZebar.Count
    }

    $wmPath = Join-Path $env:ProgramFiles 'glzr.io\GlazeWM\glazewm.exe'
    $wmProcess = Start-Process -FilePath $wmPath -WindowStyle Hidden -PassThru
    Write-SmokeLog -Event 'wm_start' -Fields @{ pid = $wmProcess.Id; path = $wmPath }
    $wmReady = Wait-ForCondition {
      try {
        $null = Get-GlobalDirection
        $true
      } catch {
        $false
      }
    }
    Assert-Smoke 'GlazeWM restarts after soft wm-exit' $wmReady
    if ($wmReady) {
      Start-SpotcoZebar | Out-Null
      $spotcoReady = Wait-ForCondition {
        try {
          $matches = @(Get-IgnoredWindows | Where-Object {
              $_.identity.processName -eq 'zebar' -and
              $_.identity.titleHint -like '*spotco.tokyo-silence*'
            })
          $matches.Count -gt 0
        } catch {
          $false
        }
      }
      Assert-Smoke 'spotco Zebar bar returns after WM restart' $spotcoReady
    }
  } catch {
    $script:FailureCount += 1
    Write-SmokeLog -Event 'soft_exit_fatal_error' -Fields @{ message = $_.Exception.Message }
  }
}

$passed = $script:FailureCount -eq 0
Write-SmokeLog -Event 'run_complete' -Fields @{
  pass = $passed
  failures = $script:FailureCount
  log = $LogPath
}
if (-not $passed) {
  exit 1
}
