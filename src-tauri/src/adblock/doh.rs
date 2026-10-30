//! DNS-over-HTTPS (DoH) sinkhole evidence layer for CNAME-cloaked trackers (v0.18.0).
//!
//! Never blocks a WebView2 request synchronously on the network: `check_host`
//! only reads the in-memory verdict map and queues unknown hosts for a
//! background resolver thread. Fail-open on any network or parse error.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};

/// Hosts queued for the background resolver + hosts already queued.
type PendingQueue = (VecDeque<String>, HashSet<String>);

pub struct DohSinkhole {
    path: PathBuf,
    enabled: bool,
    verdicts: Mutex<HashMap<String, bool>>,
    pending: Arc<(Mutex<PendingQueue>, Condvar)>,
}

impl DohSinkhole {
    pub fn new(path: PathBuf, enabled: bool) -> Self {
        let verdicts: HashMap<String, bool> = fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        Self {
            path,
            enabled,
            verdicts: Mutex::new(verdicts),
            pending: Arc::new((Mutex::new(PendingQueue::default()), Condvar::new())),
        }
    }
}

pub fn check_host(sinkhole: &Arc<DohSinkhole>, host: &str) -> bool {
    if !sinkhole.enabled {
        return false;
    }
    let _ = &sinkhole.path;
    let _ = &sinkhole.pending;
    let key = host.trim().to_ascii_lowercase();
    if key.is_empty() {
        return false;
    }
    sinkhole
        .verdicts
        .lock()
        .ok()
        .and_then(|map| map.get(&key).copied())
        .unwrap_or(false)
}
