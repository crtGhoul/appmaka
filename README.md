# AppMaka

Turn any website into a desktop web app. Add a site by name + URL, and it
opens in its own dedicated window — no browser tabs, no address bar clutter.

**Status: launcher build.** Multi-account isolation, popup blocking,
network-level ad blocking, and auto-suspend are in. This build adds launcher
mode: press **Alt+Space** anywhere to summon AppMaka, type to fuzzy-search
your web apps, accounts, and installed programs, and hit Enter.

## What exists

- **Launcher mode** — press **Alt+Space** anywhere (changeable) to summon a
  small spotlight overlay: one search field, one ranked list of your web
  apps, their accounts, and installed native programs. ↑/↓ to move, Enter
  to open/launch, Esc clears, then hides the window again. Management lives
  one click away: a "Manage apps" button in the overlay and "Show library"
  in the tray menu open the full library window.
- **Library window** — add/remove web apps, add/remove accounts per app,
  per-app settings (popups, ad blocking, auto-suspend), and launcher
  settings (hotkey, run at startup, program rescan). Opened from the tray
  menu or the overlay; the hotkey always returns to the spotlight view.
- **Native programs** — on Windows, AppMaka scans the Start Menu shortcuts
  (all-users + per-user) and Desktop shortcuts (per-user + public), resolves each `.lnk` to its target `.exe`,
  de-dupes, and extracts the exe's icon to PNG (best-effort; a letter glyph
  otherwise). On Linux it reads `.desktop` files. Programs launch through
  the OS (`ShellExecuteW` / `gio`), never by raw path from the frontend —
  launching is by id with a server-side lookup. Microsoft Store (UWP) apps
  aren't listed yet.
- **Tray icon** — left-click toggles the launcher overlay; the menu offers
  Show library, Rescan programs, and Quit. Closing the main window hides it to the tray
  instead of quitting (Quit lives in the tray menu).
- **Run at startup** — optional; toggled in the Launcher settings panel.
- **Web app icons** — taken from the site's own `/favicon.ico`; when that fails a generic
  letter glyph is shown. Empty state is honest: "No apps yet."
- **Isolated accounts** — multiple signed-in accounts per site, each in its
  own storage partition (per-webview data directories on Windows/WebView2),
  so Account A and Account B never share cookies.
- **Popup blocking** — new-window requests are blocked by default, with a
  per-site allowlist for legitimate flows (e.g. OAuth sign-in opens in a
  contained modal).
- **Network-level ad blocking** — requests are matched against EasyList and
  EasyPrivacy before they download (Windows network hook; cosmetic rules
  on all platforms).
- **Auto-suspend** — idle account windows are suspended to save RAM.
- **Add / remove / open** — add an app by pasting its URL (the name is taken
  from the page title, falling back to a prettified domain) or manually with
  name + URL (URLs are validated and normalized; `example.com` becomes
  `https://example.com/`), remove with a confirmation, open an app in its
  own window.
- **Dedicated app windows** — each account opens in its own Tauri
  `WebviewWindow` pointed at the site URL: no URL bar, just the site content,
  with the native OS window frame and the app name as the title.
  Re-opening focuses the existing window instead of duplicating it.
- **Persistence** — the app list is stored as JSON (`apps.json`) in the Tauri
  app-data directory via Rust commands (`list_apps`, `add_app`,
  `remove_app`). Writes go through a temp file + rename so a crash can't
  leave a half-written file. A corrupt `apps.json` is backed up next to
  itself (`apps.json.corrupt-<timestamp>.bak`) and replaced with a fresh
  empty library.
- **No credential handling** — you sign in directly on each site's own page
  inside its app window. If you choose Save on an account, that sign-in is
  stored by the OS credential manager (Windows Credential Manager / Secret
  Service), never in AppMaka's own files, and fills only on the same domain.
- **Clipboard history is local and literal** — it records everything you
  copy, including passwords, in AppMaka's data folder on this PC. It is
  never sent anywhere; clear it any time in Settings → Clipboard.

## Project layout

```
appmaka/
  src/                 React + TypeScript frontend (Vite)
    App.tsx            Spotlight overlay + library UI (views, quick-add, app grid)
    Launcher.tsx       Search, results list, launcher settings panel
    types.ts           Shared frontend types (backend contract)
  src-tauri/
    src/main.rs        Tauri entry: commands, tray, hotkey, window events
    src/launcher.rs    Native program scan + icon extraction + launching
    src/launcher_settings.rs  Summon hotkey + autostart (launcher.json)
    src/page_title.rs  Best-effort page-title fetch for quick-add
    src/store.rs       JSON persistence, corrupt-file backup, URL validation
    tauri.conf.json    App config (main window + bundle settings)
    capabilities/      Frontend permissions (incl. creating app windows)
    icons/             Placeholder icons (not final branding)
```

## Run it

On Linux, install the system packages Tauri needs first (this is the same
list CI installs):

```sh
sudo apt-get install -y libwebkit2gtk-4.1-dev build-essential curl wget \
  file libxdo-dev libssl-dev libayatana-appindicator3-dev libasound2-dev \
  librsvg2-dev
```

```sh
npm install
npm run tauri dev     # desktop app with hot-reload
```

Checks:

```sh
npx tsc --noEmit          # frontend typecheck
npm run build             # vite production build (tsc && vite build)
cd src-tauri && cargo check        # rust backend check
cd src-tauri && cargo clippy -- -D warnings   # zero warnings required
# Windows cross-typecheck (needs the two env shims; see AGENTS.md):
cd src-tauri && AR_x86_64_pc_windows_msvc="$PWD/../msvc-ar-wrapper.sh" RC=x86_64-w64-mingw32-windres \
  cargo check --target x86_64-pc-windows-msvc
```

## What's next (not yet implemented)

1. **Microsoft Store apps** — the program scan covers classic Win32 `.exe`
   programs; UWP/Store apps (e.g. installed via the Microsoft Store) aren't
   listed yet.

## Notes

- **Invoke arg naming (bug class, fixed 2026-10-01):** Tauri converts Rust
  `snake_case` command parameters to `camelCase` for JavaScript. Invoke
  `add_account` as `{ appId: ... }`, not `{ app_id: ... }` — the mismatch
  fails at *runtime* ("missing required key appId") and TypeScript cannot
  catch it. This shipped broken once (add/open/suspend/remove account all
  used `app_id`/`account_id`) before a full audit fixed every call. Rule:
  camelCase keys in every `invoke()` args object; the convention is also
  documented at the top of `src/types.ts`.
- Tauri 2 uses the OS webview (WebView2 on Windows, WebKitGTK on Linux) —
  no bundled Chromium, which is the main structural RAM/disk saving versus
  Electron-based wrappers.
- A few sites (notably Google) sometimes refuse sign-ins inside embedded
  webviews; the fallback is opening that site in the system browser.
