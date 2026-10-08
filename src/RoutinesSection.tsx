import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { NativeProgram, WebApp } from "./types";
import { HotkeyCapture } from "./HotkeyCapture";

/**
 * Routines ("morning stack"): one keystroke opens a chosen set of apps AND
 * the right accounts. Example: "Morning" opens work Gmail + the main Muse
 * account + a dashboard.
 *
 * Backend contract: `list_routines`, `save_routine`, `delete_routine`,
 * `run_routine` (all implemented in src-tauri/src/routines.rs).
 *
 * NOTE — invoke argument naming: Rust snake_case params become camelCase in
 * JS. `save_routine(routine: Routine)` -> invoke("save_routine", { routine });
 * `delete_routine(routine_id)` -> { routineId }; `run_routine(routine_id)` ->
 * { routineId }. Struct FIELDS stay snake_case in JSON (serde), so Routine
 * uses app_id / account_id / program_id here.
 *
 * Types are defined locally and exported; the coordinator consolidates them
 * into types.ts.
 */

// ---------------------------------------------------------------------------
// Types (exported for the coordinator)
// ---------------------------------------------------------------------------

/** One item in a routine. `kind` is "account" (app_id + account_id) or "program" (program_id). */
export interface RoutineItem {
  kind: "account" | "program";
  app_id: string;
  account_id: string | null;
  program_id: string | null;
}

export interface Routine {
  id: string;
  name: string;
  /** Global hotkey like "Ctrl+Alt+M", or null for none. */
  hotkey: string | null;
  /** Window layout: "cascade" (overlap), "side_by_side" (tile as columns),
   * or "tabbed" (one tabbed window). Missing on old records means cascade. */
  layout: RoutineLayout;
  items: RoutineItem[];
}

/** Routine window layout. Serialized snake_case in routines.json. */
export type RoutineLayout = "cascade" | "side_by_side" | "tabbed";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function describeItem(
  item: RoutineItem,
  apps: WebApp[],
  programs: NativeProgram[]
): string {
  if (item.kind === "account") {
    const app = apps.find((a) => a.id === item.app_id);
    const account = app?.accounts.find((a) => a.id === item.account_id);
    const appName = app ? app.name : "Removed app";
    const label = account ? account.label : "Removed account";
    return `${appName} - ${label}`;
  }
  const program = programs.find((p) => p.id === item.program_id);
  return program ? program.name : "Removed program";
}

function errMsg(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

// ---------------------------------------------------------------------------
// Item picker: one grouped dropdown over accounts and programs
// ---------------------------------------------------------------------------

function ItemPicker({
  apps,
  programs,
  onAdd,
}: {
  apps: WebApp[];
  programs: NativeProgram[];
  onAdd: (item: RoutineItem) => void;
}) {
  const [value, setValue] = useState("");
  const hasOptions = useMemo(
    () =>
      apps.some((a) => a.accounts.length > 0) || programs.length > 0,
    [apps, programs]
  );

  function add() {
    if (!value) return;
    const [kind, first, second] = value.split(":");
    if (kind === "account" && first && second) {
      onAdd({ kind: "account", app_id: first, account_id: second, program_id: null });
    } else if (kind === "program" && first) {
      onAdd({ kind: "program", app_id: "", account_id: null, program_id: first });
    }
    setValue("");
  }

  return (
    <div style={{ display: "flex", gap: 8, marginTop: 8 }}>
      <select
        value={value}
        onChange={(e) => setValue(e.target.value)}
        aria-label="Add an app account or program"
        disabled={!hasOptions}
        style={{ flex: 1, minWidth: 0 }}
      >
        <option value="">Add an account or program…</option>
        {apps
          .filter((a) => a.accounts.length > 0)
          .map((app) => (
            <optgroup key={app.id} label={app.name}>
              {app.accounts.map((account) => (
                <option
                  key={account.id}
                  value={`account:${app.id}:${account.id}`}
                >
                  {account.label}
                </option>
              ))}
            </optgroup>
          ))}
        {programs.length > 0 && (
          <optgroup label="Programs">
            {programs.map((program) => (
              <option key={program.id} value={`program:${program.id}`}>
                {program.name}
              </option>
            ))}
          </optgroup>
        )}
      </select>
      <button onClick={add} disabled={!value}>
        Add
      </button>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Create/edit form
// ---------------------------------------------------------------------------

function RoutineForm({
  initial,
  apps,
  programs,
  onSaved,
  onCancel,
  onError,
}: {
  initial: Routine | null;
  apps: WebApp[];
  programs: NativeProgram[];
  onSaved: (routines: Routine[]) => void;
  onCancel: () => void;
  onError: (msg: string) => void;
}) {
  const [name, setName] = useState(initial?.name ?? "");
  const [hotkey, setHotkey] = useState(initial?.hotkey ?? "");
  const [layout, setLayout] = useState<RoutineLayout>(
    initial?.layout ?? "cascade"
  );
  const [items, setItems] = useState<RoutineItem[]>(initial?.items ?? []);
  const [saving, setSaving] = useState(false);

  function moveItem(index: number, delta: number) {
    setItems((prev) => {
      const next = [...prev];
      const target = index + delta;
      if (target < 0 || target >= next.length) return prev;
      [next[index], next[target]] = [next[target], next[index]];
      return next;
    });
  }

  function removeItem(index: number) {
    setItems((prev) => prev.filter((_, i) => i !== index));
  }

  async function save() {
    setSaving(true);
    onError("");
    try {
      const routine: Routine = {
        id: initial?.id ?? "",
        name: name.trim(),
        hotkey: hotkey.trim() ? hotkey.trim() : null,
        layout,
        items,
      };
      // camelCase invoke arg for the snake_case Rust param `routine`.
      const routines = await invoke<Routine[]>("save_routine", { routine });
      onSaved(routines);
    } catch (err) {
      onError(errMsg(err));
    } finally {
      setSaving(false);
    }
  }

  return (
    <div
      style={{
        border: "1px solid var(--border, #e2e2e2)",
        borderRadius: 8,
        padding: 12,
        marginTop: 12,
      }}
    >
      <h3 style={{ margin: "0 0 8px", fontSize: 15 }}>
        {initial ? "Edit routine" : "New routine"}
      </h3>
      <label style={{ display: "block", fontSize: 14, marginBottom: 8 }}>
        <span className="field-label">Name</span>
        <input
          type="text"
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder="Morning"
          maxLength={60}
          style={{ width: "100%", marginTop: 4 }}
        />
      </label>
      <label style={{ display: "block", fontSize: 14, marginBottom: 4 }}>
        <span className="field-label">Hotkey (optional)</span>
        <div style={{ marginTop: 4 }}>
          <HotkeyCapture
            value={hotkey}
            onChange={setHotkey}
            ariaLabel="Routine hotkey"
            placeholder="Click to set…"
            allowClear
          />
        </div>
      </label>
      <p className="muted small" style={{ margin: "0 0 4px" }}>
        Pressing it runs the routine from anywhere. If another shortcut
        already uses those keys, saving will tell you so you can pick
        different ones.
      </p>
      <label style={{ display: "block", fontSize: 14, marginBottom: 8 }}>
        <span className="field-label">Window layout</span>
        <select
          value={layout}
          onChange={(e) => setLayout(e.target.value as RoutineLayout)}
          style={{ width: "100%", marginTop: 4 }}
          aria-label="Routine window layout"
        >
          <option value="cascade">Cascade — windows overlap</option>
          <option value="side_by_side">
            Side by side — tile as columns
          </option>
          <option value="tabbed">Tabbed — one window, apps as tabs</option>
        </select>
      </label>
      <p className="muted small" style={{ margin: "0 0 4px" }}>
        Side by side tiles this routine's windows as equal columns across
        your current monitor, left to right in the order below. Tabbed opens
        them all as tabs in a single window instead.
      </p>
      <div style={{ marginTop: 8 }}>
        <span className="field-label" style={{ fontSize: 14 }}>
          Opens, in order ({items.length})
        </span>
        {items.length === 0 ? (
          <p className="muted small">
            Nothing yet. Add accounts or programs below.
          </p>
        ) : (
          <ul style={{ listStyle: "none", padding: 0, margin: "8px 0" }}>
            {items.map((item, i) => (
              <li
                key={`${item.kind}-${item.app_id}-${item.account_id ?? ""}-${item.program_id ?? ""}-${i}`}
                style={{
                  display: "flex",
                  alignItems: "center",
                  gap: 6,
                  padding: "6px 0",
                  borderBottom: "1px solid var(--border, #eee)",
                  fontSize: 14,
                }}
              >
                <span
                  className="muted"
                  style={{ minWidth: 18, textAlign: "right" }}
                >
                  {i + 1}.
                </span>
                <span style={{ flex: 1, minWidth: 0 }}>
                  {describeItem(item, apps, programs)}
                </span>
                <button
                  className="text-button"
                  onClick={() => moveItem(i, -1)}
                  disabled={i === 0}
                  aria-label="Move up"
                >
                  ↑
                </button>
                <button
                  className="text-button"
                  onClick={() => moveItem(i, 1)}
                  disabled={i === items.length - 1}
                  aria-label="Move down"
                >
                  ↓
                </button>
                <button
                  className="text-button danger"
                  onClick={() => removeItem(i)}
                  aria-label="Remove"
                >
                  Remove
                </button>
              </li>
            ))}
          </ul>
        )}
        <ItemPicker
          apps={apps}
          programs={programs}
          onAdd={(item) => setItems((prev) => [...prev, item])}
        />
      </div>
      <div className="form-actions" style={{ marginTop: 12 }}>
        <button onClick={save} disabled={saving || !name.trim() || items.length === 0}>
          {saving ? "Saving…" : "Save routine"}
        </button>
        <button className="text-button" onClick={onCancel} disabled={saving}>
          Cancel
        </button>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Main section
// ---------------------------------------------------------------------------

/**
 * Settings/Library section for routines. The coordinator mounts it in
 * App.tsx's settings view (a `<section className="panel">` like the
 * Launcher/Downloads panels) with the `apps` and `programs` props.
 */
export function RoutinesSection({
  apps,
  programs,
  onChanged,
}: {
  apps: WebApp[];
  programs: NativeProgram[];
  onChanged?: (routines: Routine[]) => void;
}) {
  const [routines, setRoutines] = useState<Routine[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [formOpen, setFormOpen] = useState(false);
  const [editing, setEditing] = useState<Routine | null>(null);
  const [runningId, setRunningId] = useState<string | null>(null);
  const [lastResult, setLastResult] = useState<string | null>(null);
  const [deletingId, setDeletingId] = useState<string | null>(null);

  useEffect(() => {
    invoke<Routine[]>("list_routines")
      .then(setRoutines)
      .catch((e) => setError(`Could not load routines: ${errMsg(e)}`));
  }, []);

  function startCreate() {
    setEditing(null);
    setFormOpen(true);
    setError(null);
  }

  function startEdit(routine: Routine) {
    setEditing(routine);
    setFormOpen(true);
    setError(null);
  }

  async function runNow(routine: Routine) {
    setRunningId(routine.id);
    setLastResult(null);
    setError(null);
    try {
      // camelCase invoke arg for the snake_case Rust param `routine_id`.
      const summary = await invoke<string>("run_routine", {
        routineId: routine.id,
      });
      setLastResult(`${routine.name}: ${summary}`);
    } catch (err) {
      setError(`Could not run "${routine.name}": ${errMsg(err)}`);
    } finally {
      setRunningId(null);
    }
  }

  async function removeRoutine(routine: Routine) {
    if (
      !window.confirm(
        `Delete the "${routine.name}" routine? Its hotkey is removed too.`
      )
    ) {
      return;
    }
    setDeletingId(routine.id);
    setError(null);
    try {
      // camelCase invoke arg for the snake_case Rust param `routine_id`.
      const next = await invoke<Routine[]>("delete_routine", {
        routineId: routine.id,
      });
      setRoutines(next);
      onChanged?.(next);
    } catch (err) {
      setError(`Could not delete "${routine.name}": ${errMsg(err)}`);
    } finally {
      setDeletingId(null);
    }
  }

  if (routines === null && error === null) {
    return <p className="muted small">Loading routines…</p>;
  }

  return (
    <div>
      {error && (
        <p className="form-error" role="alert">
          {error}
        </p>
      )}
      {lastResult && (
        <p className="muted small" role="status">
          {lastResult}
        </p>
      )}
      {routines && routines.length === 0 && !formOpen ? (
        <p className="muted small">
          No routines yet. A routine opens a set of apps and accounts with one
          click or one keystroke. For example, "Morning" could open your work
          email, your main chat account, and a dashboard.
        </p>
      ) : (
        <ul style={{ listStyle: "none", padding: 0, margin: "0 0 8px" }}>
          {(routines ?? []).map((routine) => (
            <li
              key={routine.id}
              style={{
                display: "flex",
                alignItems: "center",
                gap: 8,
                padding: "8px 0",
                borderBottom: "1px solid var(--border, #eee)",
              }}
            >
              <div style={{ flex: 1, minWidth: 0 }}>
                <div style={{ fontSize: 14, fontWeight: 600 }}>
                  {routine.name}
                </div>
                <div className="muted small">
                  {routine.items.length}{" "}
                  {routine.items.length === 1 ? "item" : "items"}
                  {routine.hotkey ? ` · ${routine.hotkey}` : ""}
                  {routine.layout === "side_by_side" ? " · side by side" : ""}
                  {routine.layout === "tabbed" ? " · tabbed" : ""}
                </div>
              </div>
              <button
                onClick={() => void runNow(routine)}
                disabled={runningId === routine.id}
              >
                {runningId === routine.id ? "Opening…" : "Run now"}
              </button>
              <button
                className="text-button"
                onClick={() => startEdit(routine)}
              >
                Edit
              </button>
              <button
                className="text-button danger"
                onClick={() => void removeRoutine(routine)}
                disabled={deletingId === routine.id}
              >
                Delete
              </button>
            </li>
          ))}
        </ul>
      )}
      {formOpen ? (
        <RoutineForm
          initial={editing}
          apps={apps}
          programs={programs}
          onSaved={(next) => {
            setRoutines(next);
            onChanged?.(next);
            setFormOpen(false);
            setEditing(null);
          }}
          onCancel={() => {
            setFormOpen(false);
            setEditing(null);
          }}
          onError={(msg) => setError(msg || null)}
        />
      ) : (
        <button onClick={startCreate}>New routine</button>
      )}
    </div>
  );
}
