//! Ad/tracker blocking via Brave's `adblock-rust` engine.
//!
//! Design: one `Engine` shared by all windows behind `Arc`, rebuilt on a
//! background thread whenever the filter lists refresh. Everything here fails
//! OPEN — if the lists can't be fetched, the engine fails to build, or a
//! single request errors, the request is allowed through. Blocking ads is a
//! feature; breaking pages is a bug.
//!
//! Three layers:
//! - Network blocking (Windows only): a `WebResourceRequested` hook on each
//!   WebView2 answers 403 to requests the engine flags. See
//!   `attach_network_blocking`.
//! - Cosmetic filtering (all platforms): engine-generated `display:none`
//!   selectors injected as an initialization script, so ad placeholders that
//!   arrive inline with first-party content are hidden.
//! - Scriptlets (all platforms, v0.18.4): the `+js(...)` rules from the
//!   uBlock lists, executed by AppMaka-owned implementations (see
//!   `adblock/scriptlets.rs`) injected as an initialization script. This
//!   is the layer that reaches video ads served from the same domains as
//!   the videos themselves, which network rules cannot separate.

use adblock::engine::Engine;
use adblock::lists::{FilterSet, ParseOptions};
#[cfg(windows)]
use adblock::request::Request;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager};

// The DoH layer is consulted only by the Windows request hook, so
// it compiles there and under `test` (its suite runs on every
// platform); a Linux release build doesn't carry it at all.
#[cfg(any(windows, test))]
mod doh;
/// AppMaka-owned scriptlet implementations (v0.18.4). Unconditional:
/// scriptlet injection runs on every platform, unlike network blocking.
mod scriptlets;

const EASYLIST_URL: &str = "https://easylist.to/easylist/easylist.txt";
const EASYPRIVACY_URL: &str = "https://easylist.to/easylist/easyprivacy.txt";
/// uBlock filters (v0.18.4): the lists carrying the `+js(...)` scriptlet
/// rules, including the YouTube/YouTube Music video-ad rules. Fetched at
/// runtime like EasyList; the trusted bit is granted only to these lists.
const UBLOCK_FILTERS_URL: &str =
    "https://raw.githubusercontent.com/uBlockOrigin/uAssets/master/filters/filters.txt";
const UBLOCK_PRIVACY_URL: &str =
    "https://raw.githubusercontent.com/uBlockOrigin/uAssets/master/filters/privacy.txt";
const UBLOCK_UNBREAK_URL: &str =
    "https://raw.githubusercontent.com/uBlockOrigin/uAssets/master/filters/unbreak.txt";
const UBLOCK_QUICK_FIXES_URL: &str =
    "https://raw.githubusercontent.com/uBlockOrigin/uAssets/master/filters/quick-fixes.txt";
/// Reuse cached lists younger than this; otherwise re-download.
const LIST_MAX_AGE: Duration = Duration::from_secs(48 * 3600);
/// How often the background thread re-checks the lists.
const REFRESH_INTERVAL: Duration = Duration::from_secs(6 * 3600);

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Shared adblock state, managed by Tauri. Internals are behind `Arc`s so a
/// cheap clone can be handed to the background refresh thread.
#[derive(Clone)]
pub struct AdblockState {
    engine: Arc<RwLock<Option<Arc<Engine>>>>,
    loaded: Arc<RwLock<bool>>,
    updated_at: Arc<RwLock<Option<u64>>>,
    /// True when the last refresh produced no engine at all (v0.16.1):
    /// the honest "lists couldn't be downloaded" state the banner
    /// shows, instead of claiming "still downloading" forever.
    last_refresh_failed: Arc<RwLock<bool>>,
    cache_dir: PathBuf,
    /// v0.18.0: DoH sinkhole evidence for hosts the list engine
    /// doesn't know (see adblock/doh.rs). Consulted only by the
    /// Windows request hook, so the field exists only there.
    #[cfg(windows)]
    doh: Arc<doh::DohSinkhole>,
}

impl AdblockState {
    pub fn new(app: &AppHandle) -> Result<Self, String> {
        let dir = app
            .path()
            .app_data_dir()
            .map_err(|e| format!("could not resolve app data dir: {e}"))?;
        let cache_dir = dir.join("filter_cache");
        fs::create_dir_all(&cache_dir)
            .map_err(|e| format!("could not create filter cache dir: {e}"))?;
        Ok(Self {
            engine: Arc::new(RwLock::new(None)),
            loaded: Arc::new(RwLock::new(false)),
            updated_at: Arc::new(RwLock::new(None)),
            last_refresh_failed: Arc::new(RwLock::new(false)),
            cache_dir,
            #[cfg(windows)]
            doh: Arc::new(doh::DohSinkhole::new(dir.join("doh-verdicts.json"), true)),
        })
    }

    pub fn meta_snapshot(&self) -> (bool, Option<u64>) {
        let loaded = self.loaded.read().map(|g| *g).unwrap_or(false);
        let updated_at = self.updated_at.read().ok().and_then(|g| *g);
        (loaded, updated_at)
    }

    /// Filter-list state for the UI: "loaded" once an engine exists,
    /// "failed" when a refresh completed with nothing usable (blocking
    /// is off; the loop keeps retrying), else "loading".
    pub fn lists_state(&self) -> &'static str {
        if self.loaded.read().map(|g| *g).unwrap_or(false) {
            "loaded"
        } else if self.last_refresh_failed.read().map(|g| *g).unwrap_or(false) {
            "failed"
        } else {
            "loading"
        }
    }

    /// Background loop: refresh now, then every few hours. Never panics out —
    /// a failed refresh just keeps the previous engine (or none).
    pub fn refresh_loop(&self) {
        loop {
            self.refresh_once();
            std::thread::sleep(REFRESH_INTERVAL);
        }
    }

    fn refresh_once(&self) {
        let mut lists: Vec<(String, ParseOptions)> = Vec::new();
        let mut newest = 0u64;
        // The uBlock lists are the only ones parsed with the trusted bit,
        // so only they can invoke the `trusted-*` scriptlets.
        let trusted_opts = ParseOptions {
            permissions: scriptlets::TRUSTED,
            ..Default::default()
        };
        for (file, url, opts) in [
            ("easylist.txt", EASYLIST_URL, ParseOptions::default()),
            ("easyprivacy.txt", EASYPRIVACY_URL, ParseOptions::default()),
            ("ub-filters.txt", UBLOCK_FILTERS_URL, trusted_opts),
            ("ub-privacy.txt", UBLOCK_PRIVACY_URL, trusted_opts),
            ("ub-unbreak.txt", UBLOCK_UNBREAK_URL, trusted_opts),
            ("ub-quick-fixes.txt", UBLOCK_QUICK_FIXES_URL, trusted_opts),
        ] {
            match self.load_or_fetch(file, url) {
                Ok((text, fetched_at)) => {
                    // Skip empty downloads so a truncated fetch can't wipe
                    // out blocking for every other list.
                    if !text.trim().is_empty() {
                        lists.push((text, opts));
                        newest = newest.max(fetched_at);
                    }
                }
                Err(e) => eprintln!("[appmaka] filter list {file} unavailable: {e}"),
            }
        }
        if lists.is_empty() {
            // Fail open: keep whatever engine we had (possibly none).
            // With no engine at all, record the failure so the UI can
            // say blocking is off instead of "still downloading".
            let has_engine = self.engine.read().map(|g| g.is_some()).unwrap_or(false);
            if !has_engine {
                if let Ok(mut guard) = self.last_refresh_failed.write() {
                    *guard = true;
                }
            }
            return;
        }
        // Compiling the full lists takes ~1-3s; that's why this runs off the
        // UI thread. A fresh Engine replaces the old one atomically.
        let mut set = FilterSet::new(false);
        for (text, opts) in lists {
            set.add_filter_list(text, opts);
        }
        let mut engine = Engine::new_with_filter_set(set);
        // Scriptlet code is supplied here, not fetched: the engine matches
        // the `+js(...)` rules against these owned implementations.
        engine.use_resources(scriptlets::resources());
        let engine = Arc::new(engine);
        if let Ok(mut guard) = self.engine.write() {
            *guard = Some(engine);
        }
        if let Ok(mut guard) = self.loaded.write() {
            *guard = true;
        }
        if let Ok(mut guard) = self.last_refresh_failed.write() {
            *guard = false;
        }
        if let Ok(mut guard) = self.updated_at.write() {
            *guard = Some(newest);
        }
    }

    /// Cached list text if younger than 48h, else download and cache.
    /// Returns the text plus the unix time it was fetched.
    fn load_or_fetch(&self, file: &str, url: &str) -> Result<(String, u64), String> {
        let path = self.cache_dir.join(file);
        if let Ok(meta) = fs::metadata(&path) {
            if let Ok(modified) = meta.modified() {
                if let Ok(age) = SystemTime::now().duration_since(modified) {
                    if age < LIST_MAX_AGE {
                        let text =
                            fs::read_to_string(&path).map_err(|e| format!("read cache: {e}"))?;
                        let fetched_at = modified
                            .duration_since(UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        return Ok((text, fetched_at));
                    }
                }
            }
        }
        let text = fetch_text(url)?;
        // Cache best-effort: a failed write shouldn't fail the refresh.
        let _ = fs::write(&path, &text);
        Ok((text, unix_secs()))
    }

    /// Should this request be blocked? Used by the Windows network hook.
    /// Fail-open: no engine, bad URL, or engine error => false (allow).
    /// v0.18.5: `source_url` is the window's page URL, not the
    /// subresource URL — domain-scoped `@@` exceptions (uBlock unbreak
    /// / quick-fixes, e.g. `domain=youtube.com`) only match when the
    /// engine sees the page the request came from. An empty source
    /// falls back to the request URL (the old behavior).
    #[cfg(windows)]
    pub fn check_request(
        &self,
        url: &str,
        source_url: &str,
        request_type: &str,
        method: &str,
    ) -> bool {
        let engine = match self.engine.read().ok().and_then(|g| g.clone()) {
            Some(e) => e,
            None => return false,
        };
        let source_url = if source_url.is_empty() { url } else { source_url };
        let request = match Request::new(url, source_url, request_type, method) {
            Ok(r) => r,
            Err(_) => return false,
        };
        if engine.check_network_request(&request).should_block() {
            return true;
        }
        // Second layer (v0.18.0): DNS evidence for hosts the list
        // engine doesn't know — the CNAME-cloaked trackers. Cached
        // verdicts only; an unknown host is queued for the resolver
        // thread and allowed this time, so the COM handler never
        // waits on the network (see adblock/doh.rs).
        let host = url::Url::parse(url)
            .ok()
            .filter(|u| matches!(u.scheme(), "http" | "https"))
            .and_then(|u| u.host_str().map(str::to_owned));
        match host {
            Some(host) => doh::check_host(&self.doh, &host),
            None => false,
        }
    }

    /// Engine-generated `display:none` selectors for a page, as one CSS
    /// string. Empty when no engine is loaded (fail open => no hiding).
    /// Engine-assembled scriptlet code for a page: the shared helper
    /// definitions plus one guarded call per matching `+js(...)` rule.
    /// Empty when no engine is loaded or no rule matches (fail open).
    pub fn scriptlet_js_for(&self, page_url: &str) -> String {
        let engine = match self.engine.read().ok().and_then(|g| g.clone()) {
            Some(e) => e,
            None => return String::new(),
        };
        engine.url_cosmetic_resources(page_url).injected_script
    }

    /// v0.18.5: the YouTube ad-skipper script. Host-gated in JS itself,
    /// so builders inject it into every account window; it no-ops off
    /// YouTube. Not a filter scriptlet — no list rule references it.
    pub fn yt_ad_skipper_js(&self) -> &'static str {
        scriptlets::YT_AD_SKIPPER_JS
    }

    pub fn cosmetic_css_for(&self, page_url: &str) -> String {
        let engine = match self.engine.read().ok().and_then(|g| g.clone()) {
            Some(e) => e,
            None => return String::new(),
        };
        let resources = engine.url_cosmetic_resources(page_url);
        if resources.hide_selectors.is_empty() {
            return String::new();
        }
        // Sorted for a deterministic init script (nicer for debugging).
        let mut selectors: Vec<&str> = resources.hide_selectors.iter().map(String::as_str).collect();
        selectors.sort_unstable();
        let mut css = String::new();
        for selector in selectors {
            css.push_str(selector);
            css.push_str("{display:none !important;}");
        }
        css
    }
}

fn fetch_text(url: &str) -> Result<String, String> {
    ureq::get(url)
        .timeout(Duration::from_secs(30))
        .call()
        .map_err(|e| format!("download failed: {e}"))?
        .into_string()
        .map_err(|e| format!("read body failed: {e}"))
}

// ---------------------------------------------------------------------------
// Windows network-level blocking
// ---------------------------------------------------------------------------

/// Attach a `WebResourceRequested` handler to a freshly built window so every
/// subresource request is checked against the filter engine before it leaves
/// the machine. Blocked requests get an empty 403 response.
///
/// Safety notes: the handler runs on WebView2's thread for *every* request,
/// so it must be fast, synchronous (no deferrals), and must never panic —
/// any error allows the request through. The COM handler object is
/// reference-counted by WebView2 after `add_WebResourceRequested`, so dropping
/// our handle at the end of this function is fine.
#[cfg(windows)]
pub fn attach_network_blocking(
    window: &tauri::WebviewWindow,
    state: &AdblockState,
    enabled: Arc<std::sync::atomic::AtomicBool>,
    page_url: String,
) {
    let state = state.clone();
    let _ = window.with_webview(move |platform| {
        hook_platform(platform, &state, enabled, page_url);
    });
}

/// The COM hookup itself, shared by both entry points above.
#[cfg(windows)]
fn hook_platform(
    platform: tauri::webview::PlatformWebview,
    state: &AdblockState,
    enabled: Arc<std::sync::atomic::AtomicBool>,
    page_url: String,
) {
    use webview2_com::Microsoft::Web::WebView2::Win32::*;
    use webview2_com::WebResourceRequestedEventHandler;
    use windows_core::w;

    // The COM handler closure must be 'static, so it gets an owned clone.
    let state = state.clone();

    // Fail-open: if any step of the hookup errors, the window simply gets
    // no network blocking instead of a broken webview.
    let hooked: windows_core::Result<()> = (|| {
            let core = unsafe { platform.controller().CoreWebView2() }?;
            let environment = platform.environment();
            unsafe {
                core.AddWebResourceRequestedFilter(
                    w!("*"),
                    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
                )
            }?;
            let handler = WebResourceRequestedEventHandler::create(Box::new(
                move |_sender: Option<ICoreWebView2>,
                      args: Option<ICoreWebView2WebResourceRequestedEventArgs>| {
                    // Never propagate an error out of here: fail open.
                    if enabled.load(std::sync::atomic::Ordering::Relaxed)
                        && should_block(args.as_ref(), &state, &page_url)
                    {
                        if let Some(args) = args.as_ref() {
                            block_with_empty_response(args, &environment);
                        }
                    }
                    Ok(())
                },
            ));
            let mut token: i64 = 0;
            unsafe { core.add_WebResourceRequested(&handler, &mut token) }?;
            Ok(())
        })();
    let _ = hooked;
}

/// Decide whether a single resource request should be blocked. Pure
/// fail-open: any unexpected shape or COM error => false.
#[cfg(windows)]
fn should_block(
    args: Option<&webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2WebResourceRequestedEventArgs>,
    state: &AdblockState,
    page_url: &str,
) -> bool {
    use webview2_com::Microsoft::Web::WebView2::Win32::*;
    use webview2_com::take_pwstr;
    use windows_core::PWSTR;

    let args = match args {
        Some(a) => a,
        None => return false,
    };
    // Request URI and HTTP method come back as CoTaskMem-allocated PWSTRs;
    // take_pwstr copies them into Strings and frees the buffers.
    let (uri, method) = unsafe {
        let request = match args.Request() {
            Ok(r) => r,
            Err(_) => return false,
        };
        let mut uri_pw = PWSTR::null();
        let mut method_pw = PWSTR::null();
        if request.Uri(&mut uri_pw).is_err() || uri_pw.is_null() {
            return false;
        }
        // Method is nice-to-have; a failure just falls back to empty.
        let _ = request.Method(&mut method_pw);
        let method = if method_pw.is_null() {
            String::new()
        } else {
            take_pwstr(method_pw)
        };
        (take_pwstr(uri_pw), method)
    };
    let mut context = COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL;
    let request_type = unsafe {
        args.ResourceContext(&mut context)
            .ok()
            .map(|()| resource_context_name(&context))
            .unwrap_or("other")
    };
    state.check_request(&uri, page_url, request_type, &method)
}

/// Answer a blocked request with an empty 403. Errors are swallowed: if we
/// can't even build the response, the request was already flagged, but
/// failing the handler must not hang the page.
#[cfg(windows)]
fn block_with_empty_response(
    args: &webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2WebResourceRequestedEventArgs,
    environment: &webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Environment,
) {
    use windows_core::w;
    unsafe {
        if let Ok(response) =
            environment.CreateWebResourceResponse(None, 403, w!("Blocked"), w!(""))
        {
            let _ = args.SetResponse(&response);
        }
    }
}

/// Map a WebView2 resource context to the adblock request-type vocabulary.
#[cfg(windows)]
fn resource_context_name(
    context: &webview2_com::Microsoft::Web::WebView2::Win32::COREWEBVIEW2_WEB_RESOURCE_CONTEXT,
) -> &'static str {
    use webview2_com::Microsoft::Web::WebView2::Win32::*;
    if *context == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_IMAGE {
        "image"
    } else if *context == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_SCRIPT {
        "script"
    } else if *context == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_STYLESHEET {
        "stylesheet"
    } else if *context == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_XML_HTTP_REQUEST
        || *context == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FETCH
    {
        "xmlhttprequest"
    } else if *context == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FONT {
        "font"
    } else if *context == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_MEDIA {
        "media"
    } else if *context == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_WEBSOCKET {
        "websocket"
    } else if *context == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_PING {
        "ping"
    } else if *context == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_DOCUMENT {
        "document"
    } else {
        "other"
    }
}
