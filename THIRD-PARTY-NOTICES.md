# Third-Party Notices

AppMaka itself is licensed under the PolyForm Noncommercial License 1.0.0
(see LICENSE). This file attributes third-party components distributed
with it (as direct dependencies).

## adblock (Brave adblock-rust)

- Crate: `adblock` (v0.13.x) — https://crates.io/crates/adblock
- Source: https://github.com/brave/adblock-rust
- License: **Mozilla Public License 2.0 (MPL-2.0)**
- Used for: network-level ad/tracker request blocking and cosmetic-filter
  selector generation, matched in AppMaka's native backend against the
  EasyList and EasyPrivacy filter lists.

The MPL-2.0 is a file-level copyleft license: it applies to the `adblock`
crate's own files only. AppMaka uses it as an unmodified dependency, so
AppMaka's own code remains under its own license (PolyForm Noncommercial
1.0.0). The full MPL-2.0 text is available
at https://www.mozilla.org/en-US/MPL/2.0/ and in the crate's repository.

## png

- Crate: `png` (v0.17.x) — https://crates.io/crates/png
- Source: https://github.com/image-rs/image-png
- License: **MIT OR Apache-2.0**
- Used for: encoding extracted native program icons as PNG files on Windows
  (pure Rust, no system libraries).

## windows

- Crate: `windows` (v0.62.x) — https://crates.io/crates/windows
- Source: https://github.com/microsoft/windows-rs
- License: **MIT OR Apache-2.0**
- Used for: Windows-only launcher calls — resolving Start Menu `.lnk`
  shortcuts via `IShellLinkW`, extracting exe icons via `ExtractIconExW` +
  GDI, and launching programs via `ShellExecuteW`.

## tauri-plugin-global-shortcut

- Crate: `tauri-plugin-global-shortcut` (v2.x) — https://crates.io/crates/tauri-plugin-global-shortcut
- Source: https://github.com/tauri-apps/plugins-workspace
- License: **MIT OR Apache-2.0**
- Used for: the summon hotkey (default Alt+Space) that shows/hides the
  AppMaka window from anywhere.

## tauri-plugin-autostart

- Crate: `tauri-plugin-autostart` (v2.x) — https://crates.io/crates/tauri-plugin-autostart
- Source: https://github.com/tauri-apps/plugins-workspace
- License: **MIT OR Apache-2.0**
- Used for: the optional "run when I sign in" setting.
