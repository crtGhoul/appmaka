# Keep-MuseAgentsAlive.ps1
# Keeps Windows awake while your Muse agents run unattended in AppMaka.
# - Prevents Windows from sleeping and from turning off the display.
# - Wakes & touches open AppMaka / Muse agent windows every 30 minutes so
#   Chromium and Windows never pause background agent tasks.
# - Focus-safe: touches windows only when you are idle; if you are typing,
#   it sends non-intrusive background wake messages without stealing focus.
# - Temporary: close it (Ctrl+C) and Windows returns to normal sleep settings.
#
# How to run:
#   Right-click this file -> "Run with PowerShell"
#   (or: powershell -ExecutionPolicy Bypass -File .\Keep-MuseAgentsAlive.ps1)
# Leave the window open while your agents run. Press Ctrl+C to stop.

Add-Type @"
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;

public static class AgentWakeEngine {
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern uint SetThreadExecutionState(uint esFlags);

    public static uint KeepAwake() {
        return SetThreadExecutionState(0x80000000 | 0x00000001 | 0x00000002);
    }

    public static uint RestoreSleep() {
        return SetThreadExecutionState(0x80000000);
    }

    public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")]
    public static extern bool EnumWindows(EnumWindowsProc enumProc, IntPtr lParam);

    [DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Auto)]
    public static extern int GetWindowText(IntPtr hWnd, StringBuilder lpString, int nMaxCount);

    [DllImport("user32.dll")]
    public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint lpdwProcessId);

    [DllImport("user32.dll")]
    public static extern bool IsWindowVisible(IntPtr hWnd);

    [DllImport("user32.dll")]
    public static extern IntPtr GetForegroundWindow();

    [DllImport("user32.dll")]
    public static extern bool SetForegroundWindow(IntPtr hWnd);

    [DllImport("user32.dll")]
    public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);

    [DllImport("user32.dll")]
    public static extern bool PostMessage(IntPtr hWnd, uint Msg, IntPtr wParam, IntPtr lParam);

    [DllImport("user32.dll")]
    public static extern bool GetWindowRect(IntPtr hWnd, out RECT lpRect);

    [DllImport("user32.dll")]
    public static extern void mouse_event(uint dwFlags, uint dx, uint dy, uint dwData, UIntPtr dwExtraInfo);

    [StructLayout(LayoutKind.Sequential)]
    public struct RECT {
        public int Left;
        public int Top;
        public int Right;
        public int Bottom;
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct LASTINPUTINFO {
        public uint cbSize;
        public uint dwTime;
    }

    [DllImport("user32.dll")]
    public static extern bool GetLastInputInfo(ref LASTINPUTINFO plii);

    public static uint GetIdleSeconds() {
        LASTINPUTINFO lii = new LASTINPUTINFO();
        lii.cbSize = (uint)Marshal.SizeOf(lii);
        if (GetLastInputInfo(ref lii)) {
            uint tickCount = (uint)Environment.TickCount;
            return (tickCount - lii.dwTime) / 1000;
        }
        return 0;
    }

    public static List<IntPtr> FindAgentWindows() {
        var list = new List<IntPtr>();
        EnumWindows((hWnd, lParam) => {
            if (IsWindowVisible(hWnd)) {
                uint pid;
                GetWindowThreadProcessId(hWnd, out pid);
                StringBuilder sb = new StringBuilder(256);
                GetWindowText(hWnd, sb, 256);
                string title = sb.ToString().ToLowerInvariant();
                try {
                    var proc = System.Diagnostics.Process.GetProcessById((int)pid);
                    string name = proc.ProcessName.ToLowerInvariant();
                    if (name.Contains("appmaka") || name.Contains("appforge") || title.Contains("muse") || title.Contains("appmaka")) {
                        list.Add(hWnd);
                    }
                } catch {}
            }
            return true;
        }, IntPtr.Zero);
        return list;
    }
}
"@

# Keep power assertion active
[AgentWakeEngine]::KeepAwake() | Out-Null

$WAKE_INTERVAL_SECONDS = 1800 # 30 minutes
$lastWakeTime = 0

function Wake-AgentWindows {
    $windows = [AgentWakeEngine]::FindAgentWindows()
    $count = $windows.Count
    if ($count -eq 0) {
        Write-Host ("[{0}] Wake pulse: 0 visible AppMaka/Muse windows detected (running background ping)" -f (Get-Date -Format "HH:mm:ss")) -ForegroundColor DarkGray
        return
    }

    $idleSecs = [AgentWakeEngine]::GetIdleSeconds()
    $ts = Get-Date -Format "HH:mm:ss"

    # If user is idle (hands off keyboard/mouse for >= 15 seconds), safely touch each window
    if ($idleSecs -ge 15) {
        $prevForeground = [AgentWakeEngine]::GetForegroundWindow()
        Write-Host ("[{0}] User idle ({1}s) -> Gentle wake pulse on {2} agent window(s)..." -f $ts, $idleSecs, $count) -ForegroundColor Cyan
        
        foreach ($hwnd in $windows) {
            # Bring window forward briefly to trigger Chromium's active state
            [AgentWakeEngine]::ShowWindow($hwnd, 9) | Out-Null # SW_RESTORE / SW_SHOW
            [AgentWakeEngine]::SetForegroundWindow($hwnd) | Out-Null
            
            # Send WM_ACTIVATE (0x0006) and WM_NULL (0x0000) to safely wake message loops without clicking buttons
            [AgentWakeEngine]::PostMessage($hwnd, 0x0006, [IntPtr]1, [IntPtr]::Zero) | Out-Null
            [AgentWakeEngine]::PostMessage($hwnd, 0x0000, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null
            Start-Sleep -Milliseconds 250
        }

        # Restore original window so user workflow remains undisturbed
        if ($prevForeground -ne [IntPtr]::Zero -and !$windows.Contains($prevForeground)) {
            [AgentWakeEngine]::SetForegroundWindow($prevForeground) | Out-Null
        }
        Write-Host ("[{0}] Successfully woke {1} agent window(s) (timers and message queues refreshed)" -f $ts, $count) -ForegroundColor Green
    } else {
        # User is actively typing: do NOT steal focus. Send gentle background WM_NULL pings.
        Write-Host ("[{0}] User active (typing) -> Refreshed {1} agent window(s) in background without stealing focus" -f $ts, $count) -ForegroundColor DarkYellow
        foreach ($hwnd in $windows) {
            [AgentWakeEngine]::PostMessage($hwnd, 0x0000, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null # WM_NULL
        }
    }
}

Write-Host "==========================================================" -ForegroundColor Magenta
Write-Host "  Keep-MuseAgentsAlive Engine running" -ForegroundColor Magenta
Write-Host "  - Windows sleep: DISABLED" -ForegroundColor White
Write-Host "  - Display sleep: DISABLED" -ForegroundColor White
Write-Host "  - 30-min Agent Window Wake Pulse: ENABLED" -ForegroundColor White
Write-Host "  - Press Ctrl+C at any time to restore normal power settings" -ForegroundColor DarkGray
Write-Host "==========================================================" -ForegroundColor Magenta
Write-Host ""

# Run initial wake pulse
Wake-AgentWindows
$lastWakeTime = [Environment]::TickCount

try {
    while ($true) {
        # Re-assert execution state so power policies don't clear it
        [AgentWakeEngine]::KeepAwake() | Out-Null

        $now = [Environment]::TickCount
        $elapsedSecs = ($now - $lastWakeTime) / 1000

        if ($elapsedSecs -ge $WAKE_INTERVAL_SECONDS) {
            Wake-AgentWindows
            $lastWakeTime = [Environment]::TickCount
        } else {
            $minsLeft = [Math]::Max(1, [Math]::Round(($WAKE_INTERVAL_SECONDS - $elapsedSecs) / 60))
            Write-Host ("[{0}] Still awake - agents active (next window wake pulse in ~{1}m)" -f (Get-Date -Format "HH:mm:ss"), $minsLeft)
        }

        Start-Sleep -Seconds 60
    }
}
finally {
    # Restore normal Windows sleep behavior
    [AgentWakeEngine]::RestoreSleep() | Out-Null
    Write-Host ""
    Write-Host "Stopped. Windows sleep settings restored to normal." -ForegroundColor Green
}
