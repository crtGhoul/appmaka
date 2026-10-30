import { useEffect, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { WebApp } from "./types";

/**
 * Global "hotkey-fired" dispatcher (v0.8.0).
 *
 * The backend's centralized hotkey registry emits `hotkey-fired` with
 * `{ binding_id, kind, target }` whenever a named binding (routine /
 * workspace / per-command hotkey) is pressed. This hook listens for it and
 * dispatches through the existing invoke commands — it creates no windows
 * itself (see the Windows WebView2 deadlock note on `open_account`).
 *
 * Mount ONCE on an always-mounted surface (App.tsx's main view is the
 * natural home; see /tmp/hotkeys/INTEGRATION.md for the exact insertion
 * spec). `apps` is read through a ref so the listener never re-subscribes.
 */

export interface HotkeyFiredPayload {
  binding_id: string;
  kind: "routine" | "workspace" | "command";
  target: {
    routine_id?: string;
    workspace_id?: string;
    app_id?: string;
    account_id?: string | null;
  };
}

function errMsg(err: unknown): string {
  return typeof err === "string" ? err : "Something went wrong.";
}

export function useHotkeyDispatch(
  apps: WebApp[],
  onError: (msg: string) => void
) {
  const appsRef = useRef(apps);
  appsRef.current = apps;
  const onErrorRef = useRef(onError);
  onErrorRef.current = onError;

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let alive = true;
    void listen<HotkeyFiredPayload>("hotkey-fired", (event) => {
      if (!alive) return;
      const fail = (err: unknown) => onErrorRef.current(errMsg(err));
      const { kind, target } = event.payload;
      if (kind === "routine" && target.routine_id) {
        // Sibling-owned command (routines worker, v0.8.0).
        void invoke("run_routine", { routineId: target.routine_id }).catch(fail);
      } else if (kind === "workspace" && target.workspace_id) {
        // Sibling-owned command (workspaces worker, v0.8.0).
        void invoke("set_active_workspace", {
          workspaceId: target.workspace_id,
        }).catch(fail);
      } else if (kind === "command" && target.app_id) {
        if (target.account_id) {
          void invoke("open_account", {
            appId: target.app_id,
            accountId: target.account_id,
          }).catch(fail);
        } else {
          // App-level binding (no account): open its first account, the
          // same thing a tile click does for an app's default account.
          const app = appsRef.current.find((a) => a.id === target.app_id);
          const first = app?.accounts[0];
          if (app && first) {
            void invoke("open_account", {
              appId: app.id,
              accountId: first.id,
            }).catch(fail);
          } else {
            onErrorRef.current(
              `Couldn't open ${app?.name ?? target.app_id}: it has no accounts yet.`
            );
          }
        }
      }
    }).then(
      (u) => {
        unlisten = u;
      },
      () => {}
    );
    return () => {
      alive = false;
      unlisten?.();
    };
  }, []);
}
