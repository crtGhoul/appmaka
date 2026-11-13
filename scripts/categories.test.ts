// Unit tests for src/categories.ts — the launcher auto-sections.
// Run with: npm test   (node --test, no extra dependencies)
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  categorizeApp,
  effectiveSectionName,
  sectionOfResult,
} from "../src/categories.ts";
import type { SearchResult } from "../src/Launcher.ts";

test("AI services land in AI", () => {
  assert.equal(categorizeApp("Muse", "https://claude.ai"), "ai");
  assert.equal(categorizeApp("ChatGPT", "https://chat.openai.com"), "ai");
  assert.equal(categorizeApp("Gemini", "https://gemini.google.com"), "ai");
  assert.equal(categorizeApp("NotebookLM", "https://notebooklm.google.com"), "ai");
});

test("games land in Games", () => {
  assert.equal(categorizeApp("Steam", "https://store.steampowered.com"), "games");
  assert.equal(categorizeApp("Roblox", "https://www.roblox.com"), "games");
  assert.equal(categorizeApp("Discord", "https://discord.com"), "games");
});

test("music beats media for YouTube Music, plain YouTube stays media", () => {
  assert.equal(categorizeApp("YouTube Music", "https://music.youtube.com"), "music");
  assert.equal(categorizeApp("Spotify", "https://open.spotify.com"), "music");
  assert.equal(categorizeApp("YouTube", "https://www.youtube.com"), "media");
  assert.equal(categorizeApp("Netflix", "https://www.netflix.com"), "media");
  assert.equal(categorizeApp("CineHD", "https://cinehd.vc"), "media");
});

test("unknown apps land in Other", () => {
  assert.equal(categorizeApp("My Bank", "https://example-bank.com"), "other");
  assert.equal(categorizeApp("LocalDrop", "http://localhost:3000"), "other");
});

test("override wins over the heuristic", () => {
  const app = { name: "ChatGPT", url: "https://chat.openai.com", section: "Games" };
  assert.equal(effectiveSectionName(app, []), "Games");
  // Built-in override is normalized to the stored spelling.
  assert.equal(effectiveSectionName({ ...app, section: "games" }, []), "Games");
  // No override -> the heuristic decides.
  assert.equal(
    effectiveSectionName({ name: "ChatGPT", url: "https://chat.openai.com" }, []),
    "AI"
  );
});

test("unknown override falls back to the heuristic", () => {
  // A custom section that was removed no longer exists, so the tile
  // goes back to auto instead of stranding on a dead name.
  const app = { name: "Spotify", url: "https://open.spotify.com", section: "Work" };
  assert.equal(effectiveSectionName(app, []), "Music");
  // Same name present in customSections is honored.
  assert.equal(effectiveSectionName(app, ["Work"]), "Work");
  // Custom override is normalized to the stored spelling.
  assert.equal(effectiveSectionName({ ...app, section: "work" }, ["Work"]), "Work");
});

test("sectionOfResult routes every tile kind", () => {
  // The function only reads kind + app/program, so minimal shapes do.
  const appResult = {
    kind: "app",
    app: { name: "ChatGPT", url: "https://chat.openai.com", section: "Music" },
  } as unknown as SearchResult;
  assert.equal(sectionOfResult(appResult, []), "Music");
  const programResult = {
    kind: "program",
    program: { name: "Spotify" },
  } as unknown as SearchResult;
  assert.equal(sectionOfResult(programResult, []), "Music");
  const routineResult = {
    kind: "routine",
  } as unknown as SearchResult;
  assert.equal(sectionOfResult(routineResult, []), "Other");
});
