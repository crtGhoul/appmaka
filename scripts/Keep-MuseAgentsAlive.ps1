# Keep-MuseAgentsAlive.ps1
# Keeps Windows awake while your Muse agents run unattended in AppMaka.
# - Prevents Windows from sleeping and from turning off the display.
# - Does NOT permanently change your power plan: the effect lasts only
#   while this script is running. Close it (Ctrl+C) and Windows goes back
#   to your normal sleep settings.
#
# How to run:
#   Right-click this file -> "Run with PowerShell"
#   (or: powershell -ExecutionPolicy Bypass -File .\Keep-MuseAgentsAlive.ps1)
# Leave the window open while your agents run. Press Ctrl+C to stop.

Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class Power {
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern uint SetThreadExecutionState(uint esFlags);
}
"@

# ES_CONTINUOUS (0x80000000) = keep the setting until we clear it.
# ES_SYSTEM_REQUIRED (0x00000001) = system must not sleep.
# ES_DISPLAY_REQUIRED (0x00000002) = display must not turn off.
# ES_AWAYMODE_REQUIRED (0x00000040) = keep the machine "awake" for background work.
$ES_CONTINUOUS       = 0x80000000
$ES_SYSTEM_REQUIRED  = 0x00000001
$ES_DISPLAY_REQUIRED = 0x00000002
$ES_AWAYMODE_REQUIRED = 0x00000040

$flags = $ES_CONTINUOUS -bor $ES_SYSTEM_REQUIRED -bor $ES_DISPLAY_REQUIRED -bor $ES_AWAYMODE_REQUIRED
[Power]::SetThreadExecutionState($flags) | Out-Null

Write-Host "Keep-MuseAgentsAlive is running."
Write-Host "Windows will not sleep and the display will not turn off while this window is open."
Write-Host "Press Ctrl+C to stop and restore your normal sleep settings."
Write-Host ""

try {
    while ($true) {
        # Re-assert every 60 seconds so nothing else can quietly clear it.
        [Power]::SetThreadExecutionState($flags) | Out-Null
        Write-Host ("[{0}] still awake - agents keep running" -f (Get-Date -Format "HH:mm:ss"))
        Start-Sleep -Seconds 60
    }
}
finally {
    # Clear the request so Windows returns to its normal sleep behavior.
    [Power]::SetThreadExecutionState($ES_CONTINUOUS) | Out-Null
    Write-Host "Stopped. Windows sleep settings are back to normal."
}
