#Requires -Version 5.1
<#
.SYNOPSIS
  AppMaka Windows debug probe (CI, manual trigger).

  Seeds a fresh profile, serves a local test page, launches the DEBUG
  binary with the E2E launcher + quit flows, and measures:
    1. launcher summon + focus + typing (the [e2e] launcher flow)
    2. clean quit time (the [e2e] quit flow; FAIL if the process does
       not exit within the timeout)
    3. idle CPU over 30s for appmaka.exe + msedgewebview2.exe
    4. window move via SetWindowPos on the main window (title "AppMaka")
    5. errors.json contents after the run

  Exits 0 only if every check passes. Writes debug-out/results.md and
  debug-out/results.json, and leaves the app log + errors.json in
  debug-out/. The app log lines are the ground truth; the numbers are
  reported, not vibes.

  What this can and cannot prove: it measures CPU, quit time, focus, and
  errors on real Windows with a real WebView2. It cannot judge "feel",
  it cannot do real mouse clicks or drags (the move check is
  programmatic SetWindowPos, not a mouse drag), and the hotkey check is
  the internal toggle path, not a physical key press.
#>

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$outDir = Join-Path $repoRoot 'debug-out'
$appData = Join-Path $env:APPDATA 'com.appforge.app'
$exe = Join-Path $repoRoot 'src-tauri\target\debug\appmaka.exe'
$secHtml = Join-Path $repoRoot 'scripts\windows-debug-sec.html'
$wwwDir = Join-Path $env:TEMP 'am-win-debug-www'
$dlDir = Join-Path $env:TEMP 'am-win-debug-dl'
$appLog = Join-Path $outDir 'app.log'
$appOutLog = Join-Path $outDir 'app-stdout.log'
$httpPort = 8901
$quitAfterSecs = 90
$cpuSampleSecs = 30
$results = [ordered]@{}

function Write-ProbeLog {
    param([string]$Message)
    $ts = (Get-Date).ToString('HH:mm:ss')
    Write-Host "[$ts] $Message"
}

function Add-Result {
    param([string]$Name, [bool]$Pass, [string]$Detail)
    $results[$Name] = @{ status = $(if ($Pass) { 'PASS' } else { 'FAIL' }); detail = $Detail }
    Write-ProbeLog "$(if ($Pass) { 'PASS' } else { 'FAIL' }): $Name -- $Detail"
}

function Get-AppLogContent {
    try {
        if (Test-Path $appLog) {
            return [System.IO.File]::ReadAllText($appLog)
        }
    } catch {
        # The app holds the log open for writing; a sharing violation
        # just means "not readable yet" — retry on the next poll.
    }
    return ''
}

function Get-CpuSnapshot {
    $snap = @{}
    foreach ($name in @('appmaka', 'msedgewebview2')) {
        $sum = 0.0
        $count = 0
        $procs = Get-Process -Name $name -ErrorAction SilentlyContinue
        foreach ($p in $procs) {
            try {
                $sum += $p.TotalProcessorTime.TotalSeconds
                $count++
            } catch {
            }
        }
        $snap[$name] = @{ seconds = $sum; count = $count }
    }
    return $snap
}

# ---------------------------------------------------------------------------
# Preflight
# ---------------------------------------------------------------------------
New-Item -ItemType Directory -Force -Path $outDir | Out-Null
New-Item -ItemType Directory -Force -Path $wwwDir | Out-Null
New-Item -ItemType Directory -Force -Path $dlDir | Out-Null

if (-not (Test-Path $exe)) {
    Write-ProbeLog "FAIL: debug binary missing: $exe"
    exit 1
}
if (-not (Test-Path $secHtml)) {
    Write-ProbeLog "FAIL: test page missing: $secHtml"
    exit 1
}
$py = Get-Command python -ErrorAction SilentlyContinue
if (-not $py) {
    $py = Get-Command py -ErrorAction SilentlyContinue
}
if (-not $py) {
    Write-ProbeLog 'FAIL: python not found (needed to serve the local test page)'
    exit 1
}
Write-ProbeLog "debug binary: $exe"
Write-ProbeLog "python: $($py.Source)"

# Fresh profile: remove any stale app data so the run is reproducible.
# Also stop any stray app processes left by an earlier run.
Get-Process -Name 'appmaka' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Get-Process -Name 'msedgewebview2' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Seconds 2
if (Test-Path $appData) {
    Remove-Item -Recurse -Force $appData -ErrorAction SilentlyContinue
}
New-Item -ItemType Directory -Force -Path $appData | Out-Null

# ---------------------------------------------------------------------------
# Seed apps.json + launcher.json (same shape as the Linux live tests).
# Four example apps (tiles only) plus the "sec" app at a local URL, which
# the launcher flow opens for the Now Playing bar + focus checks.
# ---------------------------------------------------------------------------
$appsJson = @'
[
  {"id": "ex1", "name": "Example One", "url": "https://example.com", "icon": null, "color": "#57606a",
   "settings": {"auto_suspend_minutes": 1},
   "accounts": [{"id": "ex1-a", "app_id": "ex1", "label": "Main", "color": "#57606a", "session_dir": "ex1-a"}]},
  {"id": "ex2", "name": "Example Two", "url": "https://example.org", "icon": null, "color": "#57606a",
   "settings": {"auto_suspend_minutes": 1},
   "accounts": [{"id": "ex2-a", "app_id": "ex2", "label": "Main", "color": "#57606a", "session_dir": "ex2-a"}]},
  {"id": "ex3", "name": "Example Three", "url": "https://example.net", "icon": null, "color": "#57606a",
   "settings": {"auto_suspend_minutes": 1},
   "accounts": [{"id": "ex3-a", "app_id": "ex3", "label": "Main", "color": "#57606a", "session_dir": "ex3-a"}]},
  {"id": "ex4", "name": "Example Four", "url": "https://neverssl.com", "icon": null, "color": "#57606a",
   "settings": {"auto_suspend_minutes": 1},
   "accounts": [{"id": "ex4-a", "app_id": "ex4", "label": "Main", "color": "#57606a", "session_dir": "ex4-a"}]},
  {"id": "sec", "name": "Security Probe", "url": "http://127.0.0.1:8901/sec.html", "icon": null, "color": "#57606a",
   "accounts": [{"id": "sec-a", "app_id": "sec", "label": "Main", "color": "#57606a", "session_dir": "sec-a"}]}
]
'@
[System.IO.File]::WriteAllText((Join-Path $appData 'apps.json'), $appsJson)
[System.IO.File]::WriteAllText((Join-Path $appData 'launcher.json'), '{"hotkey": "Alt+Space", "seen_intro": true}')
Write-ProbeLog 'seeded apps.json (4 example + sec) + launcher.json'

# ---------------------------------------------------------------------------
# Serve the local test page.
# ---------------------------------------------------------------------------
Copy-Item $secHtml (Join-Path $wwwDir 'sec.html') -Force
$httpLog = Join-Path $outDir 'http-server.log'
$server = Start-Process -FilePath $py.Source -ArgumentList '-m', 'http.server', "$httpPort", '--bind', '127.0.0.1' -WorkingDirectory $wwwDir -PassThru -NoNewWindow -RedirectStandardOutput $httpLog -RedirectStandardError (Join-Path $outDir 'http-server-err.log')
$serverUp = $false
for ($i = 0; $i -lt 30; $i++) {
    try {
        $r = Invoke-WebRequest -Uri "http://127.0.0.1:$httpPort/sec.html" -UseBasicParsing -TimeoutSec 2 -ErrorAction Stop
        if ($r.StatusCode -eq 200) { $serverUp = $true; break }
    } catch {
    }
    Start-Sleep -Seconds 1
}
if (-not $serverUp) {
    Write-ProbeLog 'FAIL: local test page server did not come up'
    try { Stop-Process -Id $server.Id -Force -ErrorAction SilentlyContinue } catch { }
    exit 1
}
Write-ProbeLog "test page up: http://127.0.0.1:$httpPort/sec.html"

# ---------------------------------------------------------------------------
# Launch the app with the E2E launcher + quit flows.
# The debug binary is a console-subsystem build, so stderr (the [e2e]
# lines) is captured to app.log.
# ---------------------------------------------------------------------------
$env:APPMAKA_DEBUG_LAUNCHER_MODE = 'launcher'
$env:APPMAKA_DEBUG_QUIT_AFTER_SECS = "$quitAfterSecs"
$appStart = Get-Date
$appProc = Start-Process -FilePath $exe -PassThru -NoNewWindow -RedirectStandardOutput $appOutLog -RedirectStandardError $appLog -WorkingDirectory $repoRoot
Write-ProbeLog "app launched pid=$($appProc.Id) (quit fires at +${quitAfterSecs}s)"

# ---------------------------------------------------------------------------
# 1. Launcher flow: wait for "flow done", then assert the logged lines.
# ---------------------------------------------------------------------------
$flowDone = $false
$flowDeadline = (Get-Date).AddSeconds(180)
while ((Get-Date) -lt $flowDeadline) {
    $content = Get-AppLogContent
    if ($content -and $content.Contains('launcher: flow done')) { $flowDone = $true; break }
    if ($appProc.HasExited) { break }
    Start-Sleep -Seconds 2
}
$log = Get-AppLogContent
if (-not $flowDone) {
    Add-Result 'launcher-flow' $false 'launcher flow did not finish within 180s (or the app exited early)'
} else {
    Add-Result 'launcher-flow' $true 'launcher flow finished'
}

# Bar mounted (Now Playing bar present after the media toggles).
if ($log -match 'launcher: bar_present=true') {
    Add-Result 'launcher-bar' $true 'Now Playing bar mounted'
} else {
    Add-Result 'launcher-bar' $false 'Now Playing bar did not mount'
}

# No routine-hotkey perf entry in errors.json (v0.18.14 behaviour).
if ($log -match 'launcher: summon_perf_logged=false') {
    Add-Result 'launcher-no-perf-log' $true 'no Launcher summon timing entry'
} else {
    Add-Result 'launcher-no-perf-log' $false 'summon perf entry present or not measured'
}

# Summon latency + document focus. summon_focus_ms stays 2000 when the
# window never reports focused within the 2s poll window.
$summonMs = $null
$docFocused = ''
if ($log -match 'launcher: summon_focus_ms=(\d+) doc_focused=(\w+)') {
    $summonMs = [int]$Matches[1]
    $docFocused = $Matches[2]
}
if ($null -ne $summonMs -and $summonMs -lt 2000 -and $docFocused -eq 'true') {
    Add-Result 'launcher-summon' $true "summon_focus_ms=${summonMs} doc_focused=true"
} else {
    Add-Result 'launcher-summon' $false "summon_focus_ms=$summonMs doc_focused=$docFocused (want <2000 + true)"
}

# Idle polling: list_open_account_windows must not climb during the 10s idle.
$pollDelta = $null
if ($log -match 'list_open_calls_before=(\d+) after=(\d+) delta=(\d+)') {
    $pollDelta = [int]$Matches[3]
}
if ($null -ne $pollDelta -and $pollDelta -le 1) {
    Add-Result 'launcher-no-poll' $true "list_open delta=$pollDelta over 10s idle"
} else {
    Add-Result 'launcher-no-poll' $false "list_open delta=$pollDelta (want 0-1)"
}

# Typing in the launcher search input + tile filtering.
$typedOk = $false
$tilesOk = $false
$tilesDetail = ''
if ($log -match 'launcher: tiles_before=(\d+) typed="([^"]*)" tiles_after="(\d+)\|([^"]*)"') {
    $before = [int]$Matches[1]
    $typed = $Matches[2]
    $after = [int]$Matches[3]
    $labels = $Matches[4]
    $typedOk = ($typed.Contains('focused=true') -and $typed.Contains('value=Example Two'))
    $tilesOk = ($after -lt $before -and $after -ge 1)
    $tilesDetail = "tiles ${before}->${after} typed='$typed' labels='$labels'"
}
if ($typedOk -and $tilesOk) {
    Add-Result 'launcher-typing' $true $tilesDetail
} else {
    Add-Result 'launcher-typing' $false "$tilesDetail (want focused=true + value=Example Two, tiles shrink to >=1)"
}

# ---------------------------------------------------------------------------
# 2. Idle CPU over 30s (app idle: launcher + sec window open, no input).
# Reported as % of total CPU capacity (all cores), matching Task Manager.
# Thresholds are generous to avoid runner-noise false positives; the
# reported ~80% on the user's PC would be far over either one.
# ---------------------------------------------------------------------------
$cores = (Get-CimInstance Win32_ComputerSystem).NumberOfLogicalProcessors
if (-not $cores -or $cores -lt 1) { $cores = 1 }
$t0 = Get-CpuSnapshot
Start-Sleep -Seconds $cpuSampleSecs
$t1 = Get-CpuSnapshot
$appPct = (($t1['appmaka'].seconds - $t0['appmaka'].seconds) / $cpuSampleSecs / $cores) * 100
$wvPct = (($t1['msedgewebview2'].seconds - $t0['msedgewebview2'].seconds) / $cpuSampleSecs / $cores) * 100
$appPctR = [math]::Round($appPct, 2)
$wvPctR = [math]::Round($wvPct, 2)
$cpuDetail = "appmaka=${appPctR}% (n=$($t1['appmaka'].count)) msedgewebview2=${wvPctR}% (n=$($t1['msedgewebview2'].count)) over ${cpuSampleSecs}s, ${cores} cores"
if ($appPct -le 10 -and $wvPct -le 30) {
    Add-Result 'idle-cpu' $true $cpuDetail
} else {
    Add-Result 'idle-cpu' $false "$cpuDetail (want appmaka<=10% and msedgewebview2<=30%)"
}

# ---------------------------------------------------------------------------
# 3. Window move via SetWindowPos on the main window (title "AppMaka").
# Programmatic move, not a mouse drag — it proves the window can be moved
# by the OS at all. A mouse-drag failure with a passing SetWindowPos would
# point at the drag region / Tauri drag handling instead.
# ---------------------------------------------------------------------------
$win32 = @'
using System;
using System.Runtime.InteropServices;
public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
public static class Win32 {
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT lpRect);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr hWndInsertAfter, int X, int Y, int cx, int cy, uint uFlags);
}
'@
try {
    Add-Type -TypeDefinition $win32 -ErrorAction Stop
} catch {
    Add-Result 'window-move' $false "Add-Type for Win32 failed: $($_.Exception.Message)"
}
$moveDone = $false
if ($results.Contains('window-move')) {
    $moveDone = $true
}
if (-not $moveDone) {
    $hwnd = [IntPtr]::Zero
    $mainTitle = ''
    for ($i = 0; $i -lt 10; $i++) {
        $proc = Get-Process -Id $appProc.Id -ErrorAction SilentlyContinue
        if ($proc -and $proc.MainWindowHandle -ne [IntPtr]::Zero) {
            $hwnd = $proc.MainWindowHandle
            $mainTitle = $proc.MainWindowTitle
            break
        }
        Start-Sleep -Seconds 1
    }
    if ($hwnd -eq [IntPtr]::Zero) {
        Add-Result 'window-move' $false 'main window handle not found (MainWindowHandle is zero)'
    } elseif ($mainTitle -ne 'AppMaka') {
        Add-Result 'window-move' $false "main window title is '$mainTitle', not 'AppMaka' (wrong window)"
    } else {
        $rect = New-Object RECT
        $ok1 = [Win32]::GetWindowRect($hwnd, [ref]$rect)
        if (-not $ok1) {
            Add-Result 'window-move' $false 'GetWindowRect failed on the main window'
        } else {
            $oldLeft = $rect.Left
            $oldTop = $rect.Top
            $newX = $oldLeft + 120
            $newY = $oldTop + 80
            # SWP_NOSIZE (0x0001) | SWP_NOZORDER (0x0004) | SWP_NOACTIVATE (0x0010)
            $okMove = [Win32]::SetWindowPos($hwnd, [IntPtr]::Zero, $newX, $newY, 0, 0, 0x0015)
            Start-Sleep -Seconds 1
            $rect2 = New-Object RECT
            $ok2 = [Win32]::GetWindowRect($hwnd, [ref]$rect2)
            if (-not $okMove -or -not $ok2) {
                Add-Result 'window-move' $false "SetWindowPos returned $okMove, re-read returned $ok2"
            } elseif ([math]::Abs($rect2.Left - $newX) -le 5 -and [math]::Abs($rect2.Top - $newY) -le 5) {
                Add-Result 'window-move' $true "moved ($oldLeft,$oldTop) -> ($($rect2.Left),$($rect2.Top))"
            } else {
                Add-Result 'window-move' $false "window did not move: was ($oldLeft,$oldTop), asked ($newX,$newY), now ($($rect2.Left),$($rect2.Top))"
            }
        }
    }
}

# ---------------------------------------------------------------------------
# 4. Clean quit: the [e2e] quit flow fires at app start + quitAfterSecs
# and performs the exact tray-Quit sequence. Wait for the process to
# exit and measure how long after the fire time it took.
# ---------------------------------------------------------------------------
$quitFire = $appStart.AddSeconds($quitAfterSecs)
$quitDeadline = $quitFire.AddSeconds(60)
$exitTime = $null
while ((Get-Date) -lt $quitDeadline) {
    if ($appProc.HasExited) {
        $exitTime = $appProc.ExitTime
        break
    }
    Start-Sleep -Seconds 2
}
$logAfterQuit = Get-AppLogContent
$quitRequested = $logAfterQuit.Contains('quit: requested')
if ($null -eq $exitTime) {
    try { Stop-Process -Id $appProc.Id -Force -ErrorAction SilentlyContinue } catch { }
    Add-Result 'quit' $false "process did not exit within 60s after quit fired (quit log line seen: $quitRequested); force-killed"
} else {
    $quitSecs = [math]::Round(($exitTime - $quitFire).TotalSeconds, 1)
    if ($quitSecs -le 10) {
        Add-Result 'quit' $true "exited ${quitSecs}s after quit fired (quit log line seen: $quitRequested)"
    } else {
        Add-Result 'quit' $false "exited ${quitSecs}s after quit fired (slow quit; quit log line seen: $quitRequested)"
    }
}

# ---------------------------------------------------------------------------
# 5. errors.json after the run (the app redacts URLs/tokens in it).
# ---------------------------------------------------------------------------
$errorsPath = Join-Path $appData 'errors.json'
if (Test-Path $errorsPath) {
    Copy-Item $errorsPath (Join-Path $outDir 'errors.json') -Force
    $errorsText = Get-Content $errorsPath -Raw -ErrorAction SilentlyContinue
    $entryCount = -1
    try {
        $parsed = $errorsText | ConvertFrom-Json -ErrorAction Stop
        $entryCount = @($parsed).Count
    } catch {
    }
    $hasSummonTiming = $errorsText.Contains('Launcher summon timing')
    if (-not $hasSummonTiming) {
        Add-Result 'errors-json' $true "entries=$entryCount, no 'Launcher summon timing' entry"
    } else {
        Add-Result 'errors-json' $false "entries=$entryCount, 'Launcher summon timing' IS present"
    }
} else {
    Add-Result 'errors-json' $true 'no errors.json (no errors recorded)'
}

# ---------------------------------------------------------------------------
# Cleanup: stop the test page server.
# ---------------------------------------------------------------------------
try { Stop-Process -Id $server.Id -Force -ErrorAction SilentlyContinue } catch { }

# ---------------------------------------------------------------------------
# Summary: results.md + results.json, console table, exit code.
# ---------------------------------------------------------------------------
$failed = @($results.Values | Where-Object { $_.status -eq 'FAIL' }).Count
$lines = @('# Windows debug probe results', '')
foreach ($key in $results.Keys) {
    $r = $results[$key]
    $lines += "- [$($r.status)] $key -- $($r.detail)"
}
$lines += ''
$lines += "overall: $(if ($failed -eq 0) { 'ALL CHECKS PASSED' } else { "$failed check(s) FAILED" })"
[System.IO.File]::WriteAllLines((Join-Path $outDir 'results.md'), $lines)
$results | ConvertTo-Json -Depth 3 | Set-Content (Join-Path $outDir 'results.json')
Write-ProbeLog '--- summary ---'
foreach ($key in $results.Keys) {
    $r = $results[$key]
    Write-ProbeLog "[$($r.status)] $key -- $($r.detail)"
}
if ($failed -eq 0) {
    Write-ProbeLog 'ALL CHECKS PASSED'
    exit 0
} else {
    Write-ProbeLog "$failed check(s) FAILED"
    exit 1
}
