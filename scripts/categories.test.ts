// Unit tests for src/categories.ts — the launcher auto-sections.
// Run with: npm test   (node --test, no extra dependencies)
import { test } from "node:test";
import assert from "node:assert/strict";
import { categorizeApp } from "../src/categories.ts";

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
