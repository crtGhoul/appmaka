//! One-shot system actions behind a strict allowlist.
//!
//! Only the four named actions exist; anything else is rejected before any
//! process is spawned. Commands run detached (stdin/stdout/stderr nulled) so
//! e.g. `sleep` does not hold the IPC thread until the machine wakes.

use std::process::Stdio;
use tauri::AppHandle;

/// Lock, sleep, shut down, or restart the machine.
///
/// JS: `invoke("system_command", { action })`
/// where `action` is one of `"lock" | "sleep" | "shutdown" | "restart"`.
/// Anything else returns an error and runs nothing.
#[tauri::command]
pub fn system_command(action: String) -> Result<(), String> {
    match action.as_str() {
        "lock" => lock_workstation(),
        "sleep" => suspend_machine(),
        "shutdown" => power_off(),
        "restart" => reboot(),
        _ => Err("Unknown system action.".to_string()),
    }
}

fn spawn_detached(program: &str, args: &[&str]) -> Result<(), String> {
    std::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("Could not run {program}: {e}"))
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn lock_workstation() -> Result<(), String> {
    spawn_detached("rundll32.exe", &["user32.dll,LockWorkStation"])
}

#[cfg(windows)]
fn suspend_machine() -> Result<(), String> {
    spawn_detached("rundll32.exe", &["powrprof.dll,SetSuspendState 0,1,0"])
}

#[cfg(windows)]
fn power_off() -> Result<(), String> {
    spawn_detached("shutdown", &["/s", "/t", "5"])
}

#[cfg(windows)]
fn reboot() -> Result<(), String> {
    spawn_detached("shutdown", &["/r", "/t", "5"])
}

// ---------------------------------------------------------------------------
// Linux
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn lock_workstation() -> Result<(), String> {
    // Best effort: headless or non-systemd sessions may have nothing to lock.
    // Detached like every sibling: never hold the IPC thread on a child.
    spawn_detached("loginctl", &["lock-session"])
}

#[cfg(target_os = "linux")]
fn suspend_machine() -> Result<(), String> {
    spawn_detached("systemctl", &["suspend"])
}

#[cfg(target_os = "linux")]
fn power_off() -> Result<(), String> {
    spawn_detached("systemctl", &["poweroff"])
}

#[cfg(target_os = "linux")]
fn reboot() -> Result<(), String> {
    spawn_detached("systemctl", &["reboot"])
}

// ---------------------------------------------------------------------------
// Anything else
// ---------------------------------------------------------------------------

#[cfg(not(any(windows, target_os = "linux")))]
fn lock_workstation() -> Result<(), String> {
    Err("System actions are not supported on this platform.".to_string())
}

#[cfg(not(any(windows, target_os = "linux")))]
fn suspend_machine() -> Result<(), String> {
    Err("System actions are not supported on this platform.".to_string())
}

#[cfg(not(any(windows, target_os = "linux")))]
fn power_off() -> Result<(), String> {
    Err("System actions are not supported on this platform.".to_string())
}

#[cfg(not(any(windows, target_os = "linux")))]
fn reboot() -> Result<(), String> {
    Err("System actions are not supported on this platform.".to_string())
}

/// Open a URL in the system default browser. Only http/https are allowed;
/// anything else is rejected before it reaches the OS.
///
/// JS: `invoke("open_url_in_browser", { url })`
#[tauri::command]
pub fn open_url_in_browser(app: AppHandle, url: String) -> Result<(), String> {
    let parsed = url::Url::parse(&url).map_err(|_| "That link does not look valid.".to_string())?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("Only web links can be opened in the browser.".to_string());
    }
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| format!("Could not open the link: {e}"))
}
