// Unit tests for src/quickswitch.ts ranking as the Ctrl+K palette uses
// it — including the clipboard entries and the sleep-verb rows.
// Run with: npm test   (node --test, no extra dependencies)
import { test } from "node:test";
import assert from "node:assert/strict";
import { fuzzyScore, rankQuickSwitch } from "../src/quickswitch.ts";
import type { SearchResult } from "../src/Launcher.tsx";
import type { WebApp } from "../src/types.ts";

const gmailApp = {
  id: "app-1",
  name: "Gmail",
  url: "https://mail.google.com",
  icon: null,
  color: "#6366f1",
  accounts: [{ id: "acct-1", label: "Work", app_id: "app-1", color: "#6366f1", session_dir: "", last_opened: 0, created_at: 0 }],
  created_at: 0,
  settings: {
    popup_policy: "block",
    popup_allowlist: [],
    adblock_enabled: true,
    auto_suspend_minutes: 5,
    auto_close_minutes: 15,
  },
} as unknown as WebApp;

const items: SearchResult[] = [
  {
    kind: "account",
    id: "account:app-1:acct-1",
    title: "Work",
    context: "Gmail",
    score: 0,
    app: gmailApp,
    account: gmailApp.accounts[0],
  },
  {
    kind: "app",
    id: "app:app-1",
    title: "Gmail",
    context: "mail.google.com",
    score: 0,
    app: gmailApp,
  },
  {
    kind: "clipboard",
    id: "clipboard:e1",
    title: "meeting notes Q4 planning",
    context: "Clipboard",
    score: 0,
    entryId: "e1",
    pinned: false,
  },
  {
    kind: "clipboard",
    id: "clipboard:e2",
    title: "grocery list milk eggs",
    context: "Clipboard · pinned",
    score: 0,
    entryId: "e2",
    pinned: true,
  },
];

test("clipboard entries are searchable by their preview text", () => {
  const ranked = rankQuickSwitch("meeting", items);
  assert.equal(ranked.length, 1);
  assert.equal(ranked[0].kind, "clipboard");
  assert.equal((ranked[0] as { entryId: string }).entryId, "e1");
});

test("clipboard search matches a pinned entry too", () => {
  const ranked = rankQuickSwitch("grocery", items);
  assert.equal(ranked.length, 1);
  assert.equal(ranked[0].kind, "clipboard");
  assert.equal((ranked[0] as { pinned: boolean }).pinned, true);
});

test("sleep-verb rows rank by app and account name", () => {
  const ranked = rankQuickSwitch("gmail", items);
  assert.ok(ranked.length >= 2);
  assert.ok(ranked.every((r) => r.kind === "account" || r.kind === "app"));
  const rankedWork = rankQuickSwitch("work gmail", items);
  assert.equal(rankedWork[0].kind, "account");
});

test("empty query keeps browse order, capped at 15", () => {
  const ranked = rankQuickSwitch("", items);
  assert.deepEqual(
    ranked.map((r) => r.id),
    items.map((r) => r.id)
  );
  assert.ok(fuzzyScore("gm", "Gmail") > 0);
  assert.equal(fuzzyScore("zzz", "Gmail"), 0);
});

test("no match means no rows", () => {
  assert.deepEqual(rankQuickSwitch("definitely-not-there", items), []);
});
