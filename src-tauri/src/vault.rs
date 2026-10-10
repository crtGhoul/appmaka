//! OS-vault credential storage for password autofill.
//!
//! Security model (honest version):
//! - Passwords live ONLY in the OS credential store: Windows Credential
//!   Manager (DPAPI) / Linux Secret Service. Never in our JSON files, never
//!   in logs, never in error messages.
//! - Usernames are NOT secret; they live in a small sidecar JSON so the
//!   launcher can show which accounts have a saved login.
//! - Fill is NEVER automatic and NEVER cross-domain: the user clicks "Fill
//!   login" in AppMaka's own UI, and Rust refuses unless the webview's
//!   current URL host exactly matches the domain the credential was saved for.
//! - Accepted risks (same as Chrome/Bitwarden/1Password): once the password
//!   is written into the page DOM, page scripts in that origin can read it;
//!   malware running as the same OS user can call the same vault APIs.
//!   What we DO guarantee: no other local user can read them, nothing is
//!   written to disk outside the OS vault, and a phishing domain never
//!   receives a fill.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

/// Keyring service name — namespaces our entries in the OS vault.
const SERVICE: &str = "appmaka-login";

/// Non-secret metadata per account, kept in a sidecar JSON so the launcher
/// can list saved logins without touching the vault (and without ever
/// seeing a password).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct VaultIndexEntry {
    domain: String,
    username: String,
}

type VaultIndex = HashMap<String, VaultIndexEntry>;

fn index_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|d| d.join("vault-index.json"))
        .map_err(|e| format!("could not resolve app data dir: {e}"))
}

fn load_index(app: &AppHandle) -> VaultIndex {
    let Ok(path) = index_path(app) else {
        return VaultIndex::new();
    };
    let Ok(data) = fs::read(&path) else {
        return VaultIndex::new();
    };
    serde_json::from_slice(&data).unwrap_or_default()
}

fn save_index(app: &AppHandle, index: &VaultIndex) -> Result<(), String> {
    let path = index_path(app)?;
    let data =
        serde_json::to_vec_pretty(index).map_err(|e| format!("could not encode vault index: {e}"))?;
    fs::write(&path, data).map_err(|e| format!("could not write vault index: {e}"))
}

/// The vault key for an account. app_id/account_id are our own random IDs
/// (not user input), so this cannot collide across accounts — two accounts
/// on the same domain keep separate credentials.
fn vault_key(app_id: &str, account_id: &str) -> String {
    format!("{app_id}/{account_id}")
}

/// Normalize a domain for storage and comparison: lowercase, no port,
/// no trailing dot. Rejects empty hosts.
pub(crate) fn normalise_domain(raw: &str) -> Result<String, String> {
    let d = raw.trim().trim_end_matches('.').to_lowercase();
    if d.is_empty() || d.contains(['/', ':', '?', '#', '@', ' ']) {
        return Err("not a valid domain".to_string());
    }
    Ok(d)
}

/// Extract the registrable host from a URL string for fill-time matching.
fn host_of(url_str: &str) -> Result<String, String> {
    let url = url::Url::parse(url_str).map_err(|_| "could not parse page URL".to_string())?;
    url.host_str()
        .map(normalise_domain)
        .ok_or_else(|| "page URL has no host".to_string())?
}

/// The domain an app's credentials are bound to, from its configured URL.
fn app_domain(app_url: &str) -> Result<String, String> {
    host_of(app_url)
}

fn entry_for(app_id: &str, account_id: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, &vault_key(app_id, account_id))
        .map_err(|e| format!("could not access system vault: {e}"))
}

/// Save (or replace) the credential for an account. The password goes
/// straight to the OS vault; only domain+username touch our files.
/// `app_url` is the app's configured site URL — the credential is bound
/// to its domain and will never fill anywhere else.
pub(crate) fn save_credential(
    app: &AppHandle,
    app_id: &str,
    account_id: &str,
    app_url: &str,
    username: &str,
    password: &str,
) -> Result<(), String> {
    let username = username.trim();
    if username.is_empty() {
        return Err("username is empty".to_string());
    }
    if password.is_empty() {
        return Err("password is empty".to_string());
    }
    if username.len() > 512 || password.len() > 4096 {
        return Err("credential too long".to_string());
    }
    let domain = app_domain(app_url)?;

    // Vault first: if this fails nothing is recorded.
    // NOTE: the password never appears in this error string.
    entry_for(app_id, account_id)?
        .set_password(password)
        .map_err(|e| format!("could not store in system vault: {e}"))?;

    let mut index = load_index(app);
    index.insert(
        vault_key(app_id, account_id),
        VaultIndexEntry {
            domain,
            username: username.to_string(),
        },
    );
    save_index(app, &index)
}

/// Delete the credential for an account from both the vault and the index.
/// Missing entries are not an error (idempotent).
pub(crate) fn delete_credential(
    app: &AppHandle,
    app_id: &str,
    account_id: &str,
) -> Result<(), String> {
    match entry_for(app_id, account_id)?.delete_credential() {
        Ok(()) => {}
        // NotFound just means nothing was saved; anything else is real.
        Err(keyring::Error::NoEntry) => {}
        Err(e) => return Err(format!("could not delete from system vault: {e}")),
    }
    let mut index = load_index(app);
    index.remove(&vault_key(app_id, account_id));
    save_index(app, &index)
}

/// Non-secret metadata for the launcher: which accounts have saved logins
/// and for which username. Never returns a password.
pub(crate) fn credential_meta(
    app: &AppHandle,
    app_id: &str,
    account_id: &str,
) -> Option<(String, String)> {
    load_index(app)
        .get(&vault_key(app_id, account_id))
        .map(|e| (e.domain.clone(), e.username.clone()))
}

/// Fill the saved login into the account's open window.
///
/// Refuses unless:
/// - a credential is saved for this account,
/// - the account window is currently open,
/// - the webview's CURRENT url host exactly matches the saved domain.
///
/// The fill runs via webview.eval from Rust — page JavaScript can never
/// trigger it, and Tauri IPC is never exposed to the page.
pub(crate) fn fill_login(
    app: &AppHandle,
    app_id: &str,
    account_id: &str,
) -> Result<String, String> {
    let (domain, username) = credential_meta(app, app_id, account_id)
        .ok_or_else(|| "no saved login for this account".to_string())?;

    let label = format!("acct-{app_id}-{account_id}");
    let window = app
        .get_webview_window(&label)
        .ok_or_else(|| "account window is not open".to_string())?;
    let current_url = window
        .url()
        .map_err(|e| format!("could not read page URL: {e}"))?
        .to_string();
    let current_host = host_of(&current_url)?;
    if current_host != domain {
        return Err(format!(
            "refusing to fill: this page is {current_host}, the saved login is for {domain}"
        ));
    }

    // Vault read happens only after every check above has passed, so the
    // secret spends the minimum possible time in our memory.
    // NOTE: the password never appears in any error string below.
    let password = entry_for(app_id, account_id)?
        .get_password()
        .map_err(|e| format!("could not read from system vault: {e}"))?;

    let js = fill_js(&username, &password);
    // Drop the secret from our memory as soon as the script is built.
    drop(password);

    window
        .eval(&js)
        .map_err(|e| format!("could not fill the page: {e}"))?;
    Ok(format!("filled login for {username} on {domain}"))
}

/// Build the fill script. Username/password are JSON-encoded into the
/// script so quotes and backslashes cannot break out of the string
/// literals. Uses the native value setter + input/change events so
/// React/Vue/Angular forms notice the change.
fn fill_js(username: &str, password: &str) -> String {
    // serde_json::to_string on a &str always succeeds; it yields a quoted,
    // escaped JS string literal.
    let user_lit = serde_json::to_string(username).unwrap_or_else(|_| "\"\"".to_string());
    let pass_lit = serde_json::to_string(password).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        r#"(function(){{
  var user = {user_lit};
  var pass = {pass_lit};
  function setField(el, value) {{
    var setter = Object.getOwnPropertyDescriptor(
      el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype,
      "value"
    ).set;
    setter.call(el, value);
    el.dispatchEvent(new Event("input", {{ bubbles: true }}));
    el.dispatchEvent(new Event("change", {{ bubbles: true }}));
  }}
  var userField = document.querySelector(
    'input[type="email"], input[name*="user" i], input[name*="email" i], input[name*="login" i], input[autocomplete="username"], input[type="text"]'
  );
  var passField = document.querySelector('input[type="password"]');
  if (!passField) return "no password field found";
  if (userField) setField(userField, user);
  setField(passField, pass);
  passField.focus();
  return "ok";
}})()"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_normalisation() {
        assert_eq!(normalise_domain("Example.COM.").unwrap(), "example.com");
        assert_eq!(normalise_domain("  example.com ").unwrap(), "example.com");
        assert!(normalise_domain("").is_err());
        assert!(normalise_domain("example.com:8080").is_err());
        assert!(normalise_domain("http://example.com").is_err());
    }

    #[test]
    fn host_extraction() {
        assert_eq!(host_of("https://Example.COM/login?x=1").unwrap(), "example.com");
        assert_eq!(host_of("https://sub.example.com/").unwrap(), "sub.example.com");
        assert!(host_of("not a url").is_err());
    }

    #[test]
    fn fill_js_escapes_secrets() {
        // Quotes, backslashes and newlines must not break the script or
        // leak outside the string literals.
        let js = fill_js("a\"b\\c", "p@ss\n'word\"");
        assert!(js.contains("a\\\"b\\\\c"), "username not escaped");
        // The raw password must not appear verbatim anywhere.
        assert!(!js.contains("p@ss\n"), "raw password leaked");
        assert!(js.contains("input[type=\"password\"]"));
    }

    #[test]
    fn vault_key_is_namespaced() {
        assert_eq!(vault_key("app1", "acc1"), "app1/acc1");
        assert_ne!(vault_key("app1", "acc1"), vault_key("app1", "acc2"));
    }
}
