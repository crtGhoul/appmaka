# AGENTS.md — AppMaka

Binding conventions for any agent working in this project. This file governs
*how* work is done; product decisions live in chat history and release notes.

## Design direction

New direction is the user's to supply; agents transcribe, never invent a
brand. AppMaka's established look: minimal, quiet, frosted-glass launcher,
tiles with real icons, plain labels. UI built without direction must be
labeled in the handoff as "draft without direction" and is not shippable.

## Anti-slop filter (distilled, owned — applies to all UI work)

1. **Honesty first.** No invented numbers, stats, or user counts. No fake
   buttons or decorative controls. A control that doesn't work must not be
   drawn (the Attach-button lesson).
2. **Signal on the default screen.** Quiet by default: the launcher shows
   tiles; everything else moves behind panels, sections, or settings.
3. **One accent color.** Violet→maroon brand gradient only. No extra
   gradients-as-decoration, no sparkle marks, no emoji bullets.
4. **Every element earns its place.** If a control doesn't change state or
   carry a decision, it isn't drawn. Animation only when it communicates
   state.
5. **Copy is plain sentences.** Sentence case, no hype, no ALL-CAPS shouts, no
   filler. Internal names never leak into the UI.
6. **Comments explain why, never restate what.** Delete banner comments,
   emoji comments, and per-line narration of obvious code. A comment that
   survives says what the code cannot.
7. **Accessibility is part of done.** Focus visible, Esc closes dialogs,
   keyboard flows keep working, contrast checked on muted text.
8. **Self-check before handoff.** New UI gets a screenshot at 1440×900 next to
   a written pass over rules 1–7. If a rule was broken on purpose, say which
   and why.

## Code economy (distilled from ponytail — applies to all code work)

The laziest correct solution wins: write only what the task needs.

1. **Run the ladder before writing.** Skip it if it doesn't need to exist
   (YAGNI). Reuse what's already in this codebase before rewriting. Prefer
   stdlib, native platform features, and installed dependencies before adding
   new code or dependencies.
2. **Lazy about the solution, never about reading.** Read the code the change
   touches and trace the real flow before writing anything.
3. **Small because necessary, not golfed.** No clever one-liners that
   sacrifice readability; density must never obscure intent.
4. **Lazy, not negligent.** Validation at trust boundaries, data-loss
   handling, security, accessibility, and tests are never on the chopping
   block. The repo's test bar stands; this filter does not lower it.

## Win32 / FFI rules (v0.9.7 crash-loop post-mortem — binding)

v0.9.6 aborted on the FIRST page-window open, deterministically: a
`RefCell::borrow_mut()` held across `CreateWindowExW`, which synchronously
re-enters the window proc (`WM_WINDOWPOSCHANGED`), panicking on the
re-entrant borrow — and the proc had no `catch_unwind`, so the panic
unwound across `extern "system"` and killed the process. Session restore
(default ON) re-armed it every launch: crash loop. 109 unit tests and a
green Windows-target check never saw it; re-entrancy is a runtime
property of Win32, invisible in review.

1. **Every `extern "system"` function (window procs AND hook procs) MUST be
   wrapped in `std::panic::catch_unwind`.** A panic across the FFI boundary
   aborts the process instantly — no dialog, no log, no recovery. No
   exceptions, no "it can't panic". This is the cheapest insurance in
   Win32/Rust and the backstop for everything below.
2. **Win32 calls are re-entrant: NEVER hold a `RefCell`/`Mutex` borrow
   across them.** `CreateWindowExW`, `SetWindowPos`, `SendMessageW`,
   `ShowWindow`, and `DestroyWindow` synchronously deliver messages to
   your own proc (and moving an owner window echoes through its owned
   windows). Decide under a SHORT borrow, RELEASE it, then make the call
   (two-phase); re-acquire after to record results. `try_borrow()` (skip
   on contention), never `borrow()`, on any path a re-entrant proc can
   reach. Audit every `with_chrome`-style borrow site for a Win32 call
   inside it — including owner-window moves, which re-enter through the
   owned strip.
3. **Native UI construction is infallible by contract.** Any Win32
   chrome/hook failure degrades to fewer decorations (plain frameless
   window), never to a dead process. Startup must always reach a usable
   launcher no matter what failed before it: per-window `catch_unwind`
   during restore, staggered opens, failures logged to app-data
   (`caption-errors.log` — `eprintln!` is invisible on Windows GUI
   builds), never propagated.

## Hard constraints (never overridden by this file)

- License is PolyForm Noncommercial 1.0.0: source-available, commercial use
  reserved. Never relicense or add paid-tier gating without the user.
- Keep every distribution step free: no paid certs or developer accounts.
  Free OSS signing routes or click-through SmartScreen, not an EV cert.
- The in-app updater is live from v0.8.0 onward. Any release that could
  break its endpoint is a blocker, not a detail.
- No browser extensions inside account windows, ever: WebView2 (Windows) and
  WebKitGTK (Linux) have no extension model. Sites needing extensions stay in
  the real browser via the link dispatcher.
- Minimize RAM is a standing directive: the user watches Task Manager.
  Idle windows must be reclaimable; no silent background growth.

Adopted 2026-10-02 (distilled from the anti-slop and ponytail concepts; owned
text, no third-party prompt dependency).
