//! Bare-Windows-key tap to summon the clipboard popup (v0.9.2, Windows only).
//!
//! A lone Win key can't be registered with RegisterHotKey — there it's a
//! modifier, and the shell owns the bare tap for the Start menu. So this
//! uses a WH_KEYBOARD_LL hook on a dedicated thread with its own message
//! loop (the installing thread MUST pump messages or the hook silently
//! stops being called).
//!
//! How a tap is stolen without breaking Windows:
//! - The Win key-down passes through untouched, so Win+E / Win+D / Win+L /
//!   Win+Shift+S and every other combo keep working — the hook only cares
//!   about a key-up that arrives with no other key having gone down while
//!   Win was held (the "tainted" flag in TapClassifier).
//! - The Start menu opens on Win key RELEASE, not press (Raymond Chen).
//!   On a lone-tap key-up the proc swallows the release (returns nonzero,
//!   no CallNextHookEx) and SendInputs a dummy mask keystroke — inert vk
//!   0xE8, the documented AutoHotkey #MenuMaskKey trick — plus a replay of
//!   the Win key-up, both stamped in dwExtraInfo. The shell then believes
//!   Win was released *with another key held* and does not open Start, and
//!   its modifier state still clears. Our own replay is recognized by the
//!   stamp and ignored (never taints, never double-fires).
//! - Second lone tap within 400 ms: the key-up is let through untouched so
//!   the shell sees a real lone release and opens Start (double-tap =
//!   Start, the Flow Launcher pattern). Ctrl+Esc remains the keyboard
//!   fallback for Start regardless.
//!
//! The hook proc does nothing but classify + post: it try_sends a
//! HookAction and sets an event. Window work (toggle/hide) happens on the
//! hook thread's loop, never inside the proc — a proc that overruns the
//! ~300 ms LowLevelHooksTimeout is silently unhooked by Windows and the
//! Win key quietly reverts to Start.
//!
//! The hook is installed ONLY while the user opts into "Windows key
//! (single tap)" in Settings → Clipboard, and uninstalled the moment they
//! switch back to a combo. A session-wide keystroke hook has per-event
//! cost and AV eyebrows; it never runs otherwise.
//!
//! The tap classifier is pure logic (events in, actions out) and is
//! unit-tested on every platform; only the win32 machinery below it is
//! cfg(windows).

/// One decoded low-level key event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    pub vk: u32,
    pub down: bool,
    /// Stamped with our dwExtraInfo: the mask keystroke / Win-up replay we
    /// injected ourselves on a previous lone tap.
    pub injected_ours: bool,
}

/// What the hook proc should do with the raw event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    /// Pass to CallNextHookEx untouched.
    Pass,
    /// Swallow: return nonzero, do NOT call CallNextHookEx.
    Swallow,
}

/// What a classified event means for the popup, beyond pass/swallow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapOutcome {
    None,
    /// Lone Win tap completed. The proc swallows the key-up, injects the
    /// mask + Win-up replay, and the loop thread summons the popup
    /// (toggle semantics, same as the combo hotkey).
    Summon { win_vk: u32 },
    /// Second lone tap inside the double-tap window. The proc lets the
    /// key-up through so the shell opens Start; the loop thread dismisses
    /// the popup first.
    DismissForStart,
}

pub const VK_LWIN_U32: u32 = 0x5B;
pub const VK_RWIN_U32: u32 = 0x5C;
/// Second lone tap within this long after a summon opens Start instead.
const DOUBLE_TAP_MS: u64 = 400;

fn is_win_key(vk: u32) -> bool {
    vk == VK_LWIN_U32 || vk == VK_RWIN_U32
}

/// The tap state machine. Pure: feed it (event, now_ms), get back what to
/// do. `now_ms` is caller-supplied so tests can use synthetic time.
#[derive(Debug, Default)]
pub struct TapClassifier {
    win_held: bool,
    win_vk: u32,
    /// Another key went down while Win was held: this is a combo, not a
    /// tap — everything passes through untouched from here on.
    tainted: bool,
    last_summon_ms: Option<u64>,
}

impl TapClassifier {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn handle(&mut self, ev: KeyEvent, now_ms: u64) -> (KeyAction, TapOutcome) {
        // Our own injected mask/replay: never taint, never fire, never
        // swallow. It just passes through to the shell.
        if ev.injected_ours {
            return (KeyAction::Pass, TapOutcome::None);
        }
        if is_win_key(ev.vk) {
            return self.handle_win(ev.down, now_ms, ev.vk);
        }
        // Any other key: only interesting as a taint while Win is held.
        if ev.down && self.win_held {
            self.tainted = true;
        }
        (KeyAction::Pass, TapOutcome::None)
    }

    fn handle_win(&mut self, down: bool, now_ms: u64, vk: u32) -> (KeyAction, TapOutcome) {
        if down {
            if self.win_held {
                // Auto-repeat (modifiers rarely repeat, but be safe):
                // keep the current taint state, pass through.
                return (KeyAction::Pass, TapOutcome::None);
            }
            self.win_held = true;
            self.tainted = false;
            self.win_vk = vk;
            return (KeyAction::Pass, TapOutcome::None);
        }
        // Key-up.
        if !self.win_held {
            return (KeyAction::Pass, TapOutcome::None); // stray up
        }
        self.win_held = false;
        if self.tainted {
            // A combo (Win+E, Win+Shift+S, ...): untouched, as promised.
            return (KeyAction::Pass, TapOutcome::None);
        }
        let win_vk = self.win_vk;
        match self.last_summon_ms {
            Some(last) if now_ms.saturating_sub(last) < DOUBLE_TAP_MS => {
                // Double-tap: dismiss the popup and let this key-up reach
                // the shell as a real lone release → Start opens.
                self.last_summon_ms = None;
                (KeyAction::Pass, TapOutcome::DismissForStart)
            }
            _ => {
                self.last_summon_ms = Some(now_ms);
                (KeyAction::Swallow, TapOutcome::Summon { win_vk })
            }
        }
    }

}

// ---------------------------------------------------------------------------
// Win32 machinery (Windows only).
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use super::{is_win_key, KeyAction, KeyEvent, TapClassifier, TapOutcome};
    use std::sync::mpsc;
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};
    use tauri::{AppHandle, Manager};
    use windows::Win32::Foundation::{CloseHandle, HANDLE, LPARAM, LRESULT, WAIT_FAILED, WPARAM};
    use windows::Win32::System::Threading::{CreateEventW, SetEvent, INFINITE};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
        VIRTUAL_KEY,
    };
    use windows::Win32::UI::WindowsAndMessaging::*;

    /// dwExtraInfo stamp on everything we inject, so the hook recognizes
    /// its own mask keystroke / Win-up replay ("APMK").
    const STAMP: usize = 0x41504D4B;
    /// Inert virtual key for the mask keystroke (documented AutoHotkey
    /// #MenuMaskKey value; unassigned, so nothing reacts to it).
    const MASK_VK: u16 = 0xE8;

    /// Work the hook proc posts to the hook thread's loop. Never executed
    /// inside the proc itself.
    enum HookAction {
        Summon,
        Dismiss,
        Quit,
    }

    struct Installed {
        classifier: TapClassifier,
        tx: mpsc::Sender<HookAction>,
        wake: HANDLE,
    }

    // HANDLE is a raw pointer (not Send), but an Installed is only ever
    // touched under INSTALLED's mutex or on the hook thread that owns the
    // handle; the handle value itself is a process-wide token.
    unsafe impl Send for Installed {}

    static INSTALLED: Mutex<Option<Installed>> = Mutex::new(None);
    /// Serializes install/uninstall (both are rare user actions; install
    /// blocks briefly waiting for the hook thread's verdict).
    static INSTALL_LOCK: Mutex<()> = Mutex::new(());

    fn now_ms() -> u64 {
        static START: OnceLock<Instant> = OnceLock::new();
        START.get_or_init(Instant::now).elapsed().as_millis() as u64
    }

    fn key_input(vk: u16, up: bool) -> INPUT {
        let ki = KEYBDINPUT {
            wVk: VIRTUAL_KEY(vk),
            dwFlags: if up {
                KEYEVENTF_KEYUP
            } else {
                KEYBD_EVENT_FLAGS(0)
            },
            dwExtraInfo: STAMP,
            ..Default::default()
        };
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 { ki },
        }
    }

    /// On a lone-tap key-up (already swallowed): inject the mask keystroke
    /// and a replay of the Win key-up so the shell sees "released with
    /// another key held" (no Start) while its modifier state still clears.
    /// Non-blocking: SendInput just queues.
    unsafe fn inject_mask(win_vk: u32) {
        let inputs = [
            key_input(MASK_VK, false),
            key_input(MASK_VK, true),
            key_input(win_vk as u16, true),
        ];
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }

    unsafe extern "system" fn hook_proc(n_code: i32, w_param: WPARAM, l_param: LPARAM) -> LRESULT {
        // Never unwind across the FFI boundary: a panic in a hook proc
        // aborts the process (AGENTS.md Win32/FFI Rule 1). Fail open.
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            hook_proc_inner(n_code, w_param, l_param)
        })) {
            Ok(lr) => lr,
            Err(_) => CallNextHookEx(None, n_code, w_param, l_param),
        }
    }

    unsafe fn hook_proc_inner(n_code: i32, w_param: WPARAM, l_param: LPARAM) -> LRESULT {
        if n_code >= 0 {
            let w = w_param.0 as u32;
            let down = w == WM_KEYDOWN || w == WM_SYSKEYDOWN;
            let up = w == WM_KEYUP || w == WM_SYSKEYUP;
            if down || up {
                let kb = &*(l_param.0 as *const KBDLLHOOKSTRUCT);
                let injected_ours =
                    (kb.flags & LLKHF_INJECTED).0 != 0 && kb.dwExtraInfo == STAMP;
                let ev = KeyEvent {
                    vk: kb.vkCode,
                    down,
                    injected_ours,
                };
                // Classify under a short lock; everything after it
                // (SendInput, channel send, SetEvent) is non-blocking and
                // runs lock-free.
                let (action, outcome) = match INSTALLED.lock() {
                    Ok(mut guard) => match guard.as_mut() {
                        Some(inst) => {
                            let r = inst.classifier.handle(ev, now_ms());
                            let pending = match r.1 {
                                TapOutcome::Summon { .. } => Some(HookAction::Summon),
                                TapOutcome::DismissForStart => Some(HookAction::Dismiss),
                                TapOutcome::None => None,
                            };
                            if let Some(a) = pending {
                                let _ = inst.tx.send(a);
                                let _ = SetEvent(inst.wake);
                            }
                            r
                        }
                        None => (KeyAction::Pass, TapOutcome::None),
                    },
                    Err(_) => (KeyAction::Pass, TapOutcome::None),
                };
                if action == KeyAction::Swallow {
                    if let TapOutcome::Summon { win_vk } = outcome {
                        if is_win_key(win_vk) {
                            inject_mask(win_vk);
                        }
                    }
                    return LRESULT(1);
                }
            }
        }
        CallNextHookEx(None, n_code, w_param, l_param)
    }

    fn dismiss_popup(app: &AppHandle) {
        if let Some(w) = app.get_webview_window(crate::clipboard::WINDOW_LABEL) {
            let _ = w.hide();
        }
    }

    /// Hook thread: installs the LL hook, then pumps messages (mandatory —
    /// the hook is only invoked on a thread with a message loop) while
    /// waiting on the wake event for posted actions.
    fn hook_thread_main(
        app: AppHandle,
        tx: mpsc::Sender<HookAction>,
        rx: mpsc::Receiver<HookAction>,
        wake: HANDLE,
        result_tx: mpsc::Sender<Result<(), String>>,
    ) {
        let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0) };
        let hook = match hook {
            Ok(h) => h,
            Err(e) => {
                let _ = result_tx.send(Err(format!(
                    "Couldn't watch the Windows key ({e}). The combo hotkey still works."
                )));
                unsafe {
                    let _ = CloseHandle(wake);
                }
                return;
            }
        };
        {
            let mut guard = match INSTALLED.lock() {
                Ok(g) => g,
                Err(_) => {
                    unsafe {
                        let _ = UnhookWindowsHookEx(hook);
                        let _ = CloseHandle(wake);
                    }
                    let _ = result_tx.send(Err("Couldn't watch the Windows key.".to_string()));
                    return;
                }
            };
            if guard.is_some() {
                // Lost a race with another install; unhook and go away.
                drop(guard);
                unsafe {
                    let _ = UnhookWindowsHookEx(hook);
                    let _ = CloseHandle(wake);
                }
                let _ = result_tx.send(Err("The Windows-key watcher is already running.".to_string()));
                return;
            }
            *guard = Some(Installed {
                classifier: TapClassifier::new(),
                tx,
                wake,
            });
        }
        let _ = result_tx.send(Ok(()));

        let handles = [wake];
        loop {
            let waited = unsafe {
                MsgWaitForMultipleObjectsEx(Some(&handles), INFINITE, QS_ALLINPUT, MWMO_INPUTAVAILABLE)
            };
            if waited == WAIT_FAILED {
                eprintln!("winkey: message wait failed; stopping the Windows-key watcher");
                break;
            }
            let mut quit = false;
            while let Ok(action) = rx.try_recv() {
                match action {
                    HookAction::Summon => crate::clipboard::toggle_window(&app),
                    HookAction::Dismiss => dismiss_popup(&app),
                    HookAction::Quit => {
                        quit = true;
                        break;
                    }
                }
            }
            if quit {
                break;
            }
            // Pump window messages. This dispatch is also what lets the
            // system invoke hook_proc on this thread.
            unsafe {
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }
        unsafe {
            let _ = UnhookWindowsHookEx(hook);
            let _ = CloseHandle(wake);
        }
    }

    /// Install the hook. Idempotent: already-installed is Ok.
    pub fn install(app: &AppHandle) -> Result<(), String> {
        let _guard = INSTALL_LOCK
            .lock()
            .map_err(|_| "Couldn't watch the Windows key.".to_string())?;
        if INSTALLED
            .lock()
            .map(|g| g.is_some())
            .unwrap_or(false)
        {
            return Ok(());
        }
        let wake = unsafe { CreateEventW(None, false, false, None) }
            .map_err(|e| format!("Couldn't watch the Windows key ({e})."))?;
        let (tx, rx) = mpsc::channel::<HookAction>();
        let (result_tx, result_rx) = mpsc::channel::<Result<(), String>>();
        let app_c = app.clone();
        // HANDLE is a raw pointer (not Send); move it across as an integer
        // and rebuild it on the hook thread.
        let wake_bits = wake.0 as isize;
        std::thread::Builder::new()
            .name("appmaka-winkey".to_string())
            .spawn(move || hook_thread_main(app_c, tx, rx, HANDLE(wake_bits as *mut _), result_tx))
            .map_err(|e| {
                unsafe {
                    let _ = CloseHandle(wake);
                }
                format!("Couldn't start the Windows-key watcher ({e}).")
            })?;
        // The hook thread reports its SetWindowsHookExW verdict; don't
        // leave install() hanging forever if the thread died.
        result_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| {
                "Timed out watching the Windows key. The combo hotkey still works.".to_string()
            })?
    }

    /// Uninstall the hook. No-op when not installed.
    pub fn uninstall() {
        let _guard = match INSTALL_LOCK.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let inst = match INSTALLED.lock() {
            Ok(mut g) => g.take(),
            Err(_) => return,
        };
        if let Some(inst) = inst {
            // INSTALLED is already None, so the proc passes everything
            // through from here on; the thread unhooks itself on Quit.
            let _ = inst.tx.send(HookAction::Quit);
            unsafe {
                let _ = SetEvent(inst.wake);
            }
        }
    }
}

#[cfg(windows)]
pub use imp::{install, uninstall};

#[cfg(test)]
mod tests {
    use super::*;

    const E: u32 = 0x45; // 'E' — the Win+E combo probe
    const ESC: u32 = 0x1B;
    const MASK: u32 = 0xE8;

    fn down(vk: u32) -> KeyEvent {
        KeyEvent {
            vk,
            down: true,
            injected_ours: false,
        }
    }

    fn up(vk: u32) -> KeyEvent {
        KeyEvent {
            vk,
            down: false,
            injected_ours: false,
        }
    }

    fn ours(vk: u32, down: bool) -> KeyEvent {
        KeyEvent {
            vk,
            down,
            injected_ours: true,
        }
    }

    /// Drive a full sequence through a fresh classifier; returns the
    /// per-event (action, outcome) pairs. Times advance 50 ms per event
    /// unless overridden.
    fn drive(events: &[KeyEvent], t0: u64) -> Vec<(KeyAction, TapOutcome)> {
        let mut c = TapClassifier::new();
        events
            .iter()
            .enumerate()
            .map(|(i, e)| c.handle(*e, t0 + i as u64 * 50))
            .collect()
    }

    #[test]
    fn lone_tap_summons_and_swallows_keyup() {
        let out = drive(&[down(VK_LWIN_U32), up(VK_LWIN_U32)], 1000);
        assert_eq!(out[0], (KeyAction::Pass, TapOutcome::None));
        assert_eq!(out[1].0, KeyAction::Swallow);
        assert!(matches!(
            out[1].1,
            TapOutcome::Summon { win_vk } if win_vk == VK_LWIN_U32
        ));
    }

    #[test]
    fn right_win_key_taps_too() {
        let out = drive(&[down(VK_RWIN_U32), up(VK_RWIN_U32)], 1000);
        assert!(matches!(
            out[1].1,
            TapOutcome::Summon { win_vk } if win_vk == VK_RWIN_U32
        ));
    }

    #[test]
    fn win_e_combo_passes_through_untouched() {
        // The load-bearing guarantee: combos must NEVER be swallowed and
        // must never summon.
        let out = drive(
            &[down(VK_LWIN_U32), down(E), up(E), up(VK_LWIN_U32)],
            1000,
        );
        for (i, (action, outcome)) in out.iter().enumerate() {
            assert_eq!(*action, KeyAction::Pass, "event {i} must pass through");
            assert_eq!(*outcome, TapOutcome::None, "event {i} must not fire");
        }
    }

    #[test]
    fn win_shift_s_combo_passes_through() {
        let out = drive(
            &[
                down(VK_LWIN_U32),
                down(0x10), // Shift
                down(0x53), // S
                up(0x53),
                up(0x10),
                up(VK_LWIN_U32),
            ],
            1000,
        );
        for (i, (action, outcome)) in out.iter().enumerate() {
            assert_eq!(*action, KeyAction::Pass, "event {i} must pass through");
            assert_eq!(*outcome, TapOutcome::None, "event {i} must not fire");
        }
    }

    #[test]
    fn win_held_then_escape_is_tainted_not_a_tap() {
        // Press Win, think better of it, hit Escape, release Win: the
        // Escape taints the hold, so no summon and no swallow.
        let out = drive(
            &[down(VK_LWIN_U32), down(ESC), up(ESC), up(VK_LWIN_U32)],
            1000,
        );
        assert_eq!(out[3], (KeyAction::Pass, TapOutcome::None));
    }

    #[test]
    fn double_tap_within_window_dismisses_for_start() {
        let mut c = TapClassifier::new();
        c.handle(down(VK_LWIN_U32), 1000);
        let (_, o1) = c.handle(up(VK_LWIN_U32), 1100);
        assert!(matches!(o1, TapOutcome::Summon { .. }));
        c.handle(down(VK_LWIN_U32), 1300);
        // Second key-up 300 ms after the first summon: inside the window.
        let (a2, o2) = c.handle(up(VK_LWIN_U32), 1400);
        assert_eq!(a2, KeyAction::Pass, "double-tap key-up must reach the shell");
        assert_eq!(o2, TapOutcome::DismissForStart);
    }

    #[test]
    fn slow_second_tap_summons_again() {
        let mut c = TapClassifier::new();
        c.handle(down(VK_LWIN_U32), 1000);
        let (_, o1) = c.handle(up(VK_LWIN_U32), 1050);
        assert!(matches!(o1, TapOutcome::Summon { .. }));
        // 500 ms later: outside the double-tap window → toggle again.
        c.handle(down(VK_LWIN_U32), 1500);
        let (a2, o2) = c.handle(up(VK_LWIN_U32), 1550);
        assert_eq!(a2, KeyAction::Swallow);
        assert!(matches!(o2, TapOutcome::Summon { .. }));
    }

    #[test]
    fn own_injected_mask_never_taints_or_fires() {
        let mut c = TapClassifier::new();
        c.handle(down(VK_LWIN_U32), 1000);
        let (_, o) = c.handle(up(VK_LWIN_U32), 1050);
        assert!(matches!(o, TapOutcome::Summon { .. }));
        // Our injected 0xE8 down/up + Win-up replay come back through the
        // hook: all must pass, none may taint or re-fire...
        for (i, ev) in [ours(MASK, true), ours(MASK, false), ours(VK_LWIN_U32, false)]
            .iter()
            .enumerate()
        {
            let (a, o) = c.handle(*ev, 1100 + i as u64 * 10);
            assert_eq!((a, o), (KeyAction::Pass, TapOutcome::None), "injected event {i}");
        }
        // ...and a later real tap still summons (double-tap window from
        // the first summon has expired by t=2000).
        c.handle(down(VK_LWIN_U32), 2000);
        let (a, o) = c.handle(up(VK_LWIN_U32), 2050);
        assert_eq!(a, KeyAction::Swallow);
        assert!(matches!(o, TapOutcome::Summon { .. }));
    }

    #[test]
    fn win_key_repeat_down_does_not_taint() {
        let out = drive(
            &[down(VK_LWIN_U32), down(VK_LWIN_U32), up(VK_LWIN_U32)],
            1000,
        );
        assert!(matches!(
            out[2].1,
            TapOutcome::Summon { win_vk } if win_vk == VK_LWIN_U32
        ));
    }

    #[test]
    fn stray_keyup_without_down_is_ignored() {
        let out = drive(&[up(VK_LWIN_U32), down(VK_LWIN_U32), up(VK_LWIN_U32)], 1000);
        assert_eq!(out[0], (KeyAction::Pass, TapOutcome::None));
        assert!(matches!(out[2].1, TapOutcome::Summon { .. }));
    }

    #[test]
    fn plain_keys_alone_do_nothing() {
        let out = drive(&[down(0x41), up(0x41)], 1000);
        assert_eq!(out, vec![(KeyAction::Pass, TapOutcome::None); 2]);
    }

    #[test]
    fn tap_after_combo_still_summons() {
        // A combo must not leave the classifier wedged.
        let mut c = TapClassifier::new();
        for (i, ev) in [down(VK_LWIN_U32), down(E), up(E), up(VK_LWIN_U32)]
            .iter()
            .enumerate()
        {
            c.handle(*ev, 1000 + i as u64 * 50);
        }
        c.handle(down(VK_LWIN_U32), 2000);
        let (a, o) = c.handle(up(VK_LWIN_U32), 2050);
        assert_eq!(a, KeyAction::Swallow);
        assert!(matches!(o, TapOutcome::Summon { .. }));
    }
}
