//! v0.8.2: MSI-aware self-update.
//!
//! On Windows we ship two installers — the NSIS `AppMaka_{v}_x64-setup.exe`
//! and the MSI `AppMaka_{v}_x64_en-US.msi`. The updater's `latest.json` only
//! points at the NSIS one, so an app originally installed via the MSI would
//! download and run the wrong installer forever: the old MSI-installed copy
//! keeps launching and the same update is offered again and again.
//!
//! The fix: detect the install type at runtime from the running executable's
//! path. NSIS (and portable/dev) installs keep the stock
//! `downloadAndInstall` flow. MSI installs download the matching MSI, verify
//! its minisign signature against the updater public key, and launch it with
//! `msiexec /i <msi> /passive` — mirroring what the plugin does for NSIS.
//!
//! Signature verification mirrors `tauri-plugin-updater` exactly: the `.sig`
//! asset is base64 of the minisign signature text, the config pubkey is
//! base64 of the minisign public-key text, and when the trusted comment
//! carries a `version:` field it must match the announced version.

use serde::Serialize;
#[cfg(windows)]
use std::time::Duration;
use tauri::{AppHandle, Runtime};
#[cfg(windows)]
use tauri::Emitter;

/// Progress events emitted to the frontend while the MSI downloads.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Serialize)]
struct MsiProgress {
    downloaded: u64,
    total: Option<u64>,
}

/// "msi" when the running binary lives under Program Files (per-machine MSI
/// install), "nsis" for everything else (NSIS per-user install, portable,
/// dev). The frontend picks the update flow from this.
#[tauri::command]
pub fn get_install_type() -> String {
    classify_install_type(&std::env::current_exe().unwrap_or_default().to_string_lossy())
        .to_string()
}

/// Pure path classifier so it can be unit-tested with fake paths.
fn classify_install_type(exe_path: &str) -> &'static str {
    // `current_exe()` never returns a Program Files path on non-Windows, so
    // this is only true for real Windows MSI installs.
    #[cfg(windows)]
    {
        // Normalize: lowercase and strip the `\\?\` extended-path prefix so
        // the prefix check works on every Windows path form.
        let s = exe_path.to_lowercase();
        let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
        if s.starts_with(r"c:\program files\") || s.starts_with(r"c:\program files (x86)\") {
            return "msi";
        }
    }
    #[cfg(not(windows))]
    let _ = exe_path;
    "nsis"
}

/// Derive the MSI download URL from the NSIS URL in `latest.json`, e.g.
/// `.../AppMaka_0.8.2_x64-setup.exe` → `.../AppMaka_0.8.2_x64_en-US.msi`.
///
/// The WiX bundler names MSI artifacts `{product}_{version}_x64_en-US.msi`
/// (verified against the v0.8.1 release assets); only the final path segment
/// is replaced so any URL shape keeps working.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn msi_url_from_nsis_url(nsis_url: &str, version: &str) -> Result<String, String> {
    let file = format!("AppMaka_{version}_x64_en-US.msi");
    match nsis_url.rfind('/') {
        Some(i) if i + 1 < nsis_url.len() => Ok(format!("{}/{}", &nsis_url[..i], file)),
        _ => Err("couldn't work out the installer download link.".to_string()),
    }
}

/// Download the MSI, verify its signature, and hand it to the Windows
/// installer. Emits `msi-update-progress` events while downloading and
/// `msi-update-launched` right before the app exits into the installer.
/// Every failure returns a plain-language message for the Updates UI.
#[tauri::command]
pub async fn install_msi_update<R: Runtime>(
    app: AppHandle<R>,
    nsis_url: String,
    version: String,
) -> Result<(), String> {
    #[cfg(not(windows))]
    {
        let _ = (&app, nsis_url, version);
        Err("MSI updates only apply on Windows.".to_string())
    }
    #[cfg(windows)]
    {
        let app = app.clone();
        let record_app = app.clone();
        let result = tokio::task::spawn_blocking(move || run_msi_update(&app, &nsis_url, &version))
            .await
            .map_err(|e| format!("the update was interrupted ({e})."))
            .and_then(|r| r);
        if let Err(e) = &result {
            // v0.11.0: recorded for Copy diagnostics. The updater panel
            // shows the inline error itself, so no dialog here.
            crate::errors::record(
                &record_app,
                "updater",
                "The update couldn't be downloaded or installed.",
                e,
                false,
            );
        }
        result
    }
}

#[cfg(windows)]
fn run_msi_update<R: Runtime>(
    app: &AppHandle<R>,
    nsis_url: &str,
    version: &str,
) -> Result<(), String> {
    let msi_url = msi_url_from_nsis_url(nsis_url, version)?;

    let msi_bytes = download_with_progress(app, &msi_url)?;

    let sig_url = format!("{msi_url}.sig");
    let sig_bytes = download_bytes(&sig_url)?;

    let pubkey_b64 = updater_pubkey(app)?;
    verify_msi_signature(&msi_bytes, &sig_bytes, &pubkey_b64, version)?;

    let msi_path = std::env::temp_dir().join(format!("AppMaka-{version}-update.msi"));
    std::fs::write(&msi_path, &msi_bytes)
        .map_err(|_| "couldn't save the downloaded update.".to_string())?;

    launch_msiexec(&msi_path)?;

    // The installer is now running detached. Exit immediately — staying
    // alive even briefly lets Restart Manager see a process that refuses
    // close requests (the main window hides to the tray), which wedges
    // the app ("Not Responding"). The `msi-update-launched` emit is
    // best-effort; the frontend also flips to "Installer launched" when
    // the download byte count completes (see App.tsx).
    let _ = app.emit("msi-update-launched", ());
    app.exit(0);
    #[allow(unreachable_code)]
    Ok(())
}

/// Read the updater public key from the app config (the same key the NSIS
/// updater flow verifies against).
#[cfg(windows)]
fn updater_pubkey<R: Runtime>(app: &AppHandle<R>) -> Result<String, String> {
    app.config()
        .plugins
        .0
        .get("updater")
        .and_then(|v| v.get("pubkey"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| "the updater isn't configured correctly.".to_string())
}

/// Verify the downloaded MSI against its `.sig` asset, mirroring
/// `tauri-plugin-updater`: base64-decode both the config pubkey and the sig
/// file, parse them as minisign text, verify, and — when the trusted comment
/// carries a version — require it to match the announced version so a
/// tampered manifest can't pair a new version number with old bytes.
#[cfg(windows)]
fn verify_msi_signature(
    msi_bytes: &[u8],
    sig_bytes: &[u8],
    pubkey_b64: &str,
    version: &str,
) -> Result<(), String> {
    use minisign_verify::{PublicKey, Signature};

    let bad_key = || "the updater's security key looks wrong.".to_string();
    let pubkey_text =
        String::from_utf8(base64_decode(pubkey_b64).map_err(|_| bad_key())?).map_err(|_| bad_key())?;
    let public_key = PublicKey::decode(&pubkey_text).map_err(|_| bad_key())?;

    // The `.sig` asset is base64 of the minisign signature text (same bytes
    // the plugin verifies for the NSIS installer).
    let sig_text = sig_text_from_sig_bytes(sig_bytes)
        .ok_or_else(|| "the update's security check looks wrong.".to_string())?;
    let signature = Signature::decode(&sig_text)
        .map_err(|_| "the update's security check looks wrong.".to_string())?;

    public_key
        .verify(msi_bytes, &signature, true)
        .map_err(|_| "the update failed its security check — nothing was installed.".to_string())?;

    // Only the verified trusted comment is usable from here on (same rule as
    // the plugin: the global signature covers it, and `verify` above checked
    // that signature).
    if let Some(signed) = signature
        .trusted_comment()
        .split('\t')
        .find_map(|field| field.strip_prefix("version:"))
    {
        let same = signed.trim_start_matches('v') == version.trim_start_matches('v');
        if !same {
            return Err("the update failed its security check — nothing was installed.".to_string());
        }
    }
    Ok(())
}

/// Decode the `.sig` bytes into minisign signature text: base64 of the text
/// (what the Tauri signer produces), falling back to the raw text itself.
#[cfg(windows)]
fn sig_text_from_sig_bytes(sig_bytes: &[u8]) -> Option<String> {
    let trimmed: Vec<u8> = sig_bytes
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    if let Ok(decoded) = base64_decode_bytes(&trimmed) {
        if let Ok(text) = String::from_utf8(decoded) {
            if text.contains("untrusted comment:") {
                return Some(text);
            }
        }
    }
    String::from_utf8(trimmed)
        .ok()
        .filter(|text| text.contains("untrusted comment:"))
}

#[cfg(windows)]
fn base64_decode(s: &str) -> Result<Vec<u8>, base64::DecodeError> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s.trim())
}

#[cfg(windows)]
fn base64_decode_bytes(b: &[u8]) -> Result<Vec<u8>, base64::DecodeError> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(b)
}

/// Stream the MSI to memory, emitting progress events for the Updates UI.
#[cfg(windows)]
fn download_with_progress<R: Runtime>(app: &AppHandle<R>, url: &str) -> Result<Vec<u8>, String> {
    let response = ureq::get(url)
        .timeout(Duration::from_secs(30))
        .call()
        .map_err(|_| "couldn't download the update.".to_string())?;
    let total: Option<u64> = response
        .header("Content-Length")
        .and_then(|v| v.parse().ok());
    let mut reader = response.into_reader();
    let mut buf = Vec::with_capacity(total.unwrap_or(8 * 1024 * 1024) as usize);
    let mut chunk = [0u8; 64 * 1024];
    let _ = app.emit(
        "msi-update-progress",
        MsiProgress {
            downloaded: 0,
            total,
        },
    );
    loop {
        let n = std::io::Read::read(&mut reader, &mut chunk)
            .map_err(|_| "couldn't download the update.".to_string())?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        let _ = app.emit(
            "msi-update-progress",
            MsiProgress {
                downloaded: buf.len() as u64,
                total,
            },
        );
    }
    Ok(buf)
}

#[cfg(windows)]
fn download_bytes(url: &str) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    ureq::get(url)
        .timeout(Duration::from_secs(30))
        .call()
        .map_err(|_| "couldn't download the update's security check.".to_string())?
        .into_reader()
        .read_to_end(&mut buf)
        .map_err(|_| "couldn't download the update's security check.".to_string())?;
    Ok(buf)
}

/// Launch the Windows installer on the downloaded MSI, fully detached from
/// this process.
///
/// Detached + immediate caller exit is load-bearing: msiexec's Restart
/// Manager must never see a live app that refuses close requests (the
/// main window hides to the tray instead of closing), or the app wedges.
/// Absolute System32 path so PATH games can't redirect it; `/passive`
/// shows progress without asking questions.
#[cfg(windows)]
fn launch_msiexec(msi_path: &std::path::Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    /// DETACHED_PROCESS: the installer outlives us and is never our child.
    const DETACHED_PROCESS: u32 = 0x00000008;
    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
    let msiexec = format!(r"{system_root}\System32\msiexec.exe");
    std::process::Command::new(&msiexec)
        .arg("/i")
        .arg(msi_path)
        .arg("/passive")
        .creation_flags(DETACHED_PROCESS)
        .spawn()
        .map_err(|_| "couldn't start the Windows installer.".to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msi_url_replaces_only_the_file_name() {
        let url = msi_url_from_nsis_url(
            "https://github.com/crtGhoul/appforge/releases/download/v0.8.2/AppMaka_0.8.2_x64-setup.exe",
            "0.8.2",
        )
        .unwrap();
        assert_eq!(
            url,
            "https://github.com/crtGhoul/appforge/releases/download/v0.8.2/AppMaka_0.8.2_x64_en-US.msi"
        );
    }

    #[test]
    fn msi_url_rejects_url_without_path() {
        assert!(msi_url_from_nsis_url("AppMaka_0.8.2_x64-setup.exe", "0.8.2").is_err());
    }

    #[test]
    fn install_type_classification() {
        // Windows-only assertions: on other platforms everything is "nsis".
        #[cfg(windows)]
        {
            assert_eq!(
                classify_install_type(r"C:\Program Files\AppMaka\appmaka.exe"),
                "msi"
            );
            assert_eq!(
                classify_install_type(r"C:\Program Files (x86)\AppMaka\appmaka.exe"),
                "msi"
            );
            assert_eq!(
                classify_install_type(r"\\?\C:\Program Files\AppMaka\appmaka.exe"),
                "msi"
            );
            assert_eq!(
                classify_install_type(r"c:\program files\appmaka\appmaka.exe"),
                "msi"
            );
            assert_eq!(
                classify_install_type(r"C:\Users\me\AppData\Local\AppMaka\appmaka.exe"),
                "nsis"
            );
        }
        #[cfg(not(windows))]
        {
            assert_eq!(
                classify_install_type(r"C:\Program Files\AppMaka\appmaka.exe"),
                "nsis"
            );
            assert_eq!(classify_install_type("/home/me/appmaka"), "nsis");
        }
    }

    #[cfg(windows)]
    #[test]
    fn signature_rejects_tampered_sig() {
        // Well-formed base64 but not a minisign signature → must fail.
        let sig = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            "definitely not a signature",
        );
        let key = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDZDOEY5QzAzMjI1Q0M4QjkKUldTNXlGd2lBNXlQYkpHSCtNaDVSc2tXd094VmwrcGpNbVpZSlltZzBWQ2tidHoxWCtHMjlDNnkK";
        assert!(verify_msi_signature(b"bytes", sig.as_bytes(), key, "0.8.2").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn signature_rejects_bad_pubkey() {
        assert!(verify_msi_signature(b"bytes", b"!!!", "!!!", "0.8.2").is_err());
    }
}
