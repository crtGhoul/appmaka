//! Per-window "Don't close this window" switch (v0.9.9).
//!
//! The word "pin" never appears in the UI (the launcher tile menu already
//! uses "Pin to top" for tiles); internally the state is called pinned.
//! Every close attempt on a pinned page window (account windows `acct-*`
//! and the search window `websearch`) is diverted to one native confirm
//! instead of closing. Unpinning restores normal close behavior at once.
//!
//! The crash-loop sentinel outranks the pin: a stale sentinel forces
//! ask-mode and pinned windows do not force-restore into a crash. Tray
//! Quit is never blocked by pins (see `SHUTTING_DOWN`).
//!
//! Threading: all state is a short-lived Mutex. The confirm dialog runs on
//! a plain helper thread — never on the main thread, never on a
//! window-proc, hook-proc, or IPC thread.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tauri::{AppHandle, Manager};

/// Runtime pin map, keyed by window label. Managed as Tauri state; seeded
/// at startup from session.json before any restore runs.
#[derive(Debug, Default)]
pub struct PinState(pub Mutex<HashMap<String, bool>>);

/// One-shot: labels whose close the user already confirmed. Consumed by the
/// close path so the confirm never re-fires for the same close.
#[derive(Debug, Default)]
pub struct ConfirmedCloses(pub Mutex<HashSet<String>>);

/// Dedupe: a confirm is already showing for this label (Alt+F4 held down,
/// Esc auto-repeat). Later attempts no-op until it resolves.
#[derive(Debug, Default)]
pub struct PendingConfirms(pub Mutex<HashSet<String>>);

/// What kind of close is being attempted. Moved here from windows.rs so
/// the close-plan decision lives next to the pin state it reasons about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseIntent {
    /// The user asked for this close (X button, Alt+F4, gesture, menu,
    /// close-all): pinned windows get one confirm.
    User,
    /// An automatic reclaim (the idle watcher): pinned windows are skipped
    /// silently, never shown a dialog nobody is watching.
    Background,
}

/// The verdict for one close attempt. Pure — unit tests pin down the full
/// matrix without an AppHandle.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ClosePlan {
    /// Close right away: unpinned, shutting down, or already confirmed.
    Proceed,
    /// Ask the user first with the native confirm.
    Confirm,
    /// Drop the attempt silently (pinned + background intent).
    Skip,
}

pub(crate) fn plan_close(
    intent: CloseIntent,
    pinned: bool,
    shutting_down: bool,
) -> ClosePlan {
    // Tray Quit is never blocked by pins: while shutting down, every
    // close proceeds — a pin must never trap the user in the app.
    if shutting_down || !pinned {
        return ClosePlan::Proceed;
    }
    match intent {
        CloseIntent::Background => ClosePlan::Skip,
        CloseIntent::User => ClosePlan::Confirm,
    }
}

/// Pure core of the batch confirm: which labels need the one dialog.
/// `take_confirmed` consumes one-shot approvals (a previous approval for
/// this same close-all skips the dialog). Unpinned labels are not the
/// caller's concern — the caller closes those unconditionally.
pub(crate) fn batch_confirm_list<'a>(
    labels: &'a [String],
    is_pinned: &dyn Fn(&str) -> bool,
    take_confirmed: &mut dyn FnMut(&str) -> bool,
) -> Vec<&'a String> {
    labels
        .iter()
        .filter(|l| is_pinned(l.as_str()) && !take_confirmed(l.as_str()))
        .collect()
}

/// Pure core of startup seeding: pinned session entries become the window
/// labels to mark. Labels are derivable without opening any window, so
/// entries whose account no longer exists seed harmlessly and die on the
/// next session write.
pub(crate) fn seed_labels(windows: &[crate::session::SessionWindow]) -> Vec<String> {
    windows
        .iter()
        .filter(|w| w.pinned())
        .map(|w| match w {
            crate::session::SessionWindow::Account {
                app_id, account_id, ..
            } => crate::windows::account_window_label(app_id, account_id),
            crate::session::SessionWindow::Search { .. } => {
                crate::websearch::SEARCH_WINDOW_LABEL.to_string()
            }
            // v0.10.0: tabbed groups pin by stable group id, never the
            // generation-suffixed window label.
            crate::session::SessionWindow::Tabbed { id, .. } => {
                crate::tabs::pin_key(id)
            }
        })
        .collect()
}

/// The crash-loop sentinel outranks the pin: pinned windows restore on
/// launch only when the sentinel did NOT force ask-mode, and only when
/// the user isn't already in full "Restore last session" mode (which
/// restores everything, pinned or not). Pure so the rule is unit-tested.
pub(crate) fn pinned_restore_applies(sentinel_forced: bool, full_restore_mode: bool) -> bool {
    !sentinel_forced && !full_restore_mode
}

/// Set in the tray-Quit handler before anything else. Close guards fail
/// closed while this is set: a pin must never trap the user in the app or
/// strand a modal during exit.
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

pub fn set_shutting_down() {
    SHUTTING_DOWN.store(true, Ordering::SeqCst);
}

pub fn is_shutting_down() -> bool {
    SHUTTING_DOWN.load(Ordering::SeqCst)
}

pub fn is_pinned(app: &AppHandle, label: &str) -> bool {
    app.try_state::<PinState>()
        .and_then(|s| s.0.lock().ok().map(|m| m.get(label).copied().unwrap_or(false)))
        .unwrap_or(false)
}

/// Consume one confirmed close for a label. Returns true when a previous
/// confirm approved this close.
pub fn take_confirmed(app: &AppHandle, label: &str) -> bool {
    app.try_state::<ConfirmedCloses>()
        .and_then(|s| s.0.lock().ok().map(|mut set| set.remove(label)))
        .unwrap_or(false)
}

/// Seed the runtime map from the saved session before restore runs.
pub fn seed_from_session(app: &AppHandle) {
    let session = crate::session::load_session(app);
    let labels = seed_labels(&session.windows);
    if std::env::var("APPMAKA_DEBUG_PIN").is_ok() {
        eprintln!("[appmaka] pin seed: {labels:?}");
    }
    if labels.is_empty() {
        return;
    }
    let Some(state) = app.try_state::<PinState>() else {
        return;
    };
    let Ok(mut map) = state.0.lock() else {
        return;
    };
    for label in labels {
        map.insert(label, true);
    }
}

/// Persist a pin change: update the runtime map and rewrite the session so
/// the flag survives restarts. Idempotent; a missing label is a no-op
/// success (the map entry, if any, is pruned on the next session write).
pub fn set_pinned(app: &AppHandle, label: &str, pinned: bool) {
    if let Some(state) = app.try_state::<PinState>() {
        if let Ok(mut map) = state.0.lock() {
            if pinned {
                map.insert(label.to_string(), true);
            } else {
                map.remove(label);
            }
        }
    }
    crate::session::write_session(app);
}

fn show_pin_confirm(app: &AppHandle, body: &str) -> bool {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
    // blocking_show must never run on the main thread; every caller here
    // is a helper thread, a command thread, or the chrome thread.
    app.dialog()
        .message(body)
        .title("Pinned window")
        .buttons(MessageDialogButtons::OkCancelCustom(
            "Close".to_string(),
            "Keep open".to_string(),
        ))
        .blocking_show()
}

/// Fire-and-forget: the caller already prevented the close. Shows the
/// confirm on a helper thread and closes the window on "Close". Deduped per
/// label so a held Alt+F4 never stacks dialogs.
pub fn ask_then_close(app: &AppHandle, label: &str) {
    if is_shutting_down() {
        return;
    }
    let fresh = app
        .try_state::<PendingConfirms>()
        .and_then(|p| {
            p.0.lock()
                .ok()
                .map(|mut set| set.insert(label.to_string()))
        })
        .unwrap_or(false);
    if !fresh {
        return;
    }
    let app = app.clone();
    let label = label.to_string();
    let _ = std::thread::Builder::new()
        .name("appmaka-pin-confirm".to_string())
        .spawn(move || {
            let approved =
                show_pin_confirm(&app, "This window is pinned. Close it anyway?");
            if let Some(p) = app.try_state::<PendingConfirms>() {
                if let Ok(mut set) = p.0.lock() {
                    set.remove(&label);
                }
            }
            if !approved {
                return;
            }
            if let Some(c) = app.try_state::<ConfirmedCloses>() {
                if let Ok(mut set) = c.0.lock() {
                    set.insert(label.clone());
                }
            }
            if let Some(w) = app.get_webview_window(&label) {
                let _ = w.close();
            }
        });
}

/// Synchronous guard for programmatic User-intent closes. Returns true when
/// the close may proceed. Blocks the caller's thread on the confirm — safe
/// only on command or watcher threads, never the main thread and never a
/// Win32 callback. Background (unattended) callers must not use this; they
/// skip pinned windows silently instead.
pub fn guard_close(app: &AppHandle, label: &str) -> bool {
    if is_shutting_down() {
        return true;
    }
    if !is_pinned(app, label) {
        return true;
    }
    if take_confirmed(app, label) {
        return true;
    }
    let approved = show_pin_confirm(app, "This window is pinned. Close it anyway?");
    if approved {
        if let Some(c) = app.try_state::<ConfirmedCloses>() {
            if let Ok(mut set) = c.0.lock() {
                set.insert(label.to_string());
            }
        }
    }
    approved
}

/// Batch confirm for close-all: one dialog for all pinned labels, returns
/// the labels the user approved. Unpinned labels are not the caller's
/// concern here — the caller closes those unconditionally.
pub fn confirm_batch_close(app: &AppHandle, labels: &[String]) -> HashSet<String> {
    let mut approved = HashSet::new();
    if is_shutting_down() {
        return approved;
    }
    // Short sequential locks via the pure core — never nested, never held
    // across the blocking dialog below.
    let need: Vec<String> = batch_confirm_list(
        labels,
        &|l| is_pinned(app, l),
        &mut |l| take_confirmed(app, l),
    )
    .into_iter()
    .cloned()
    .collect();
    if need.is_empty() {
        return approved;
    }
    let body = format!(
        "{} of these windows are pinned. Close them anyway?",
        need.len()
    );
    if show_pin_confirm(app, &body) {
        if let Some(c) = app.try_state::<ConfirmedCloses>() {
            if let Ok(mut set) = c.0.lock() {
                set.extend(need.iter().cloned());
            }
        }
        approved.extend(need);
    }
    approved
}

/// Tauri commands (camelCase on the wire: set_window_pinned { label, pinned }).
#[tauri::command]
pub fn set_window_pinned(app: AppHandle, label: String, pinned: bool) -> Result<(), String> {
    set_pinned(&app, &label, pinned);
    Ok(())
}

#[tauri::command]
pub fn window_pinned(app: AppHandle, label: String) -> bool {
    is_pinned(&app, &label)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionWindow;

    /// pin -> close attempt -> confirm -> close/keep, plus the never-trap
    /// rules: shutdown always proceeds, background never asks.
    #[test]
    fn plan_close_matrix() {
        use CloseIntent::{Background, User};
        use ClosePlan::{Confirm, Proceed, Skip};
        // Unpinned: everything proceeds, no dialog ever.
        assert_eq!(plan_close(User, false, false), Proceed);
        assert_eq!(plan_close(Background, false, false), Proceed);
        // Pinned, user intent: one confirm.
        assert_eq!(plan_close(User, true, false), Confirm);
        // Pinned, background intent (idle watcher): skipped silently.
        assert_eq!(plan_close(Background, true, false), Skip);
        // Tray Quit is never blocked by pins: shutdown proceeds even for
        // pinned windows on every intent.
        assert_eq!(plan_close(User, true, true), Proceed);
        assert_eq!(plan_close(Background, true, true), Proceed);
        assert_eq!(plan_close(User, false, true), Proceed);
        assert_eq!(plan_close(Background, false, true), Proceed);
    }

    /// The batch confirm lists exactly the pinned labels that lack a
    /// one-shot approval, and consumes those approvals.
    #[test]
    fn batch_confirm_list_filters_and_consumes() {
        let labels = vec![
            "acct-a-1".to_string(),
            "acct-b-2".to_string(),
            "acct-c-3".to_string(),
        ];
        let pinned: HashMap<String, bool> = HashMap::from([
            ("acct-a-1".to_string(), true),
            ("acct-b-2".to_string(), true),
        ]);
        let is_pinned = |l: &str| pinned.get(l).copied().unwrap_or(false);
        // acct-b-2 already confirmed this close-all: skipped, consumed.
        let mut confirmed: HashSet<String> = HashSet::from(["acct-b-2".to_string()]);
        let need = batch_confirm_list(&labels, &is_pinned, &mut |l| confirmed.remove(l));
        assert_eq!(need, vec![&"acct-a-1".to_string()]);
        assert!(confirmed.is_empty(), "one-shot must be consumed");
        // Unpinned acct-c-3 never appears, even with no confirmations.
        let mut confirmed: HashSet<String> = HashSet::new();
        let need = batch_confirm_list(&labels, &is_pinned, &mut |l| confirmed.remove(l));
        assert_eq!(need, vec![&"acct-a-1".to_string(), &"acct-b-2".to_string()]);
    }

    #[test]
    fn batch_confirm_list_empty_when_nothing_pinned() {
        let labels = vec!["acct-a-1".to_string()];
        let mut confirmed: HashSet<String> = HashSet::new();
        let need = batch_confirm_list(&labels, &|_| false, &mut |l| confirmed.remove(l));
        assert!(need.is_empty());
    }

    /// Seeding derives labels without opening windows; unpinned entries
    /// are skipped; the search window maps to its fixed label.
    #[test]
    fn seed_labels_derives_only_pinned() {
        let windows = vec![
            SessionWindow::Account {
                app_id: "app1".to_string(),
                account_id: "acct1".to_string(),
                rect: None,
                pinned: true,
            },
            SessionWindow::Account {
                app_id: "app1".to_string(),
                account_id: "acct2".to_string(),
                rect: None,
                pinned: false,
            },
            SessionWindow::Search {
                query: "q".to_string(),
                rect: None,
                pinned: true,
            },
        ];
        let mut labels = seed_labels(&windows);
        labels.sort();
        assert_eq!(labels, vec!["acct-app1-acct1", "websearch"]);
    }

    /// The crash-loop sentinel outranks the pin: a stale sentinel forces
    /// ask-mode and pinned windows do not restore. Full restore mode
    /// restores everything anyway, so the pinned-only path is moot there.
    #[test]
    fn sentinel_outranks_pin() {
        assert!(pinned_restore_applies(false, false));
        assert!(!pinned_restore_applies(true, false));
        assert!(!pinned_restore_applies(false, true));
        assert!(!pinned_restore_applies(true, true));
    }

    #[test]
    fn batch_body_counts_pinned() {
        let n = 2usize;
        let body = format!("{n} of these windows are pinned. Close them anyway?");
        assert_eq!(body, "2 of these windows are pinned. Close them anyway?");
    }
}
