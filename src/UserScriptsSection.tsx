import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

export interface UserScriptInfo {
  id?: string;
  filename: string;
  name: string;
  description?: string | null;
  matches: string[];
  enabled: boolean;
  blocked?: boolean;
  block_reason?: string | null;
  blockReason?: string | null;
}

function errMsg(err: unknown): string {
  return typeof err === "string" ? err : "Something went wrong.";
}

/**
 * Settings section for Tampermonkey-style user scripts (v0.18.0).
 * Lists `.user.js` / `.js` files from the app's userscripts folder,
 * shows which sites each script matches, blocks scripts using risky
 * constructs, and lets the user toggle or reload scripts.
 */
export function UserScriptsSection() {
  const [scripts, setScripts] = useState<UserScriptInfo[] | null>(null);
  const [dirPath, setDirPath] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const [list, dir] = await Promise.all([
        invoke<UserScriptInfo[]>("list_user_scripts"),
        invoke<string>("get_userscripts_dir").catch(() => null),
      ]);
      setScripts(list);
      setDirPath(dir);
    } catch (err) {
      setError(errMsg(err));
      setScripts([]);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  async function handleReload() {
    setLoading(true);
    setError(null);
    try {
      const list = await invoke<UserScriptInfo[]>("reload_user_scripts");
      setScripts(list);
    } catch (err) {
      setError(errMsg(err));
    } finally {
      setLoading(false);
    }
  }

  async function handleToggle(script: UserScriptInfo, enabled: boolean) {
    const key = script.id ?? script.filename;
    setBusyId(key);
    setError(null);
    try {
      const list = await invoke<UserScriptInfo[]>("set_user_script_enabled", {
        id: key,
        filename: script.filename,
        enabled,
      });
      setScripts(list);
    } catch (err) {
      setError(errMsg(err));
    } finally {
      setBusyId(null);
    }
  }

  return (
    <div className="user-scripts-section">
      {dirPath && (
        <p className="muted small" style={{ marginTop: 8 }}>
          Scripts folder: <code>{dirPath}</code>
        </p>
      )}
      <div className="inline-form" style={{ marginTop: 10 }}>
        <button
          type="button"
          onClick={() => void handleReload()}
          disabled={loading}
        >
          {loading ? "Reloading…" : "Reload scripts"}
        </button>
      </div>

      {error && (
        <p className="form-error" role="alert">
          {error}
        </p>
      )}

      {scripts !== null && scripts.length === 0 && !error && (
        <p className="muted small" style={{ marginTop: 10 }}>
          No user scripts found. Drop a <code>.user.js</code> file with{" "}
          <code>// @match https://example.com/*</code> into the scripts folder,
          then click Reload scripts.
        </p>
      )}

      {scripts !== null && scripts.length > 0 && (
        <ul className="account-list" style={{ marginTop: 12 }}>
          {scripts.map((script) => {
            const key = script.id ?? script.filename;
            const reason = script.block_reason ?? script.blockReason ?? null;
            const blocked = Boolean(script.blocked || reason);
            const matches = script.matches ?? [];
            return (
              <li key={key} className="account-row">
                <div className="account-meta">
                  <span className="account-label-row">
                    <span className="account-label">
                      {script.name || script.filename}
                    </span>
                    <span className="muted small">{script.filename}</span>
                  </span>
                  {script.description && (
                    <span className="muted small">{script.description}</span>
                  )}
                  <span className="account-opened">
                    {matches.length > 0
                      ? `Matches: ${matches.join(", ")}`
                      : "No @match patterns declared"}
                  </span>
                  {blocked && (
                    <span className="form-error" style={{ marginTop: 4 }}>
                      Blocked{reason ? `: ${reason}` : " (risky construct)"}
                    </span>
                  )}
                </div>
                <div className="app-actions">
                  <label className="check-row">
                    <input
                      type="checkbox"
                      checked={!blocked && script.enabled}
                      disabled={blocked || busyId === key}
                      onChange={(e) =>
                        void handleToggle(script, e.target.checked)
                      }
                    />
                    <span>{blocked ? "Blocked" : "Enabled"}</span>
                  </label>
                </div>
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}

export default UserScriptsSection;
