/**
 * v0.15.0+: Ctrl+K quick-switch, prebuilt fuzzy entries (v0.17.0), and
 * the persisted search-index cache payload helpers (v0.18.0).
 */
import type { SearchResult } from "./Launcher";
import type { NativeProgram } from "./types";

/**
 * Prebuilt per-text index so keystroke scoring never re-lowercases or
 * re-scans word boundaries across thousands of programs.
 */
export interface FuzzyEntry {
  text: string;
  lower: string;
  wordStarts: number[];
  acronym: string;
}

function isBoundaryChar(ch: string): boolean {
  return ch === " " || ch === "-" || ch === "_" || ch === "." || ch === "/";
}

export function buildFuzzyEntry(text: string): FuzzyEntry {
  const lower = text.toLowerCase();
  const wordStarts: number[] = [];
  let acronym = "";
  for (let i = 0; i < lower.length; i++) {
    if (i === 0 || isBoundaryChar(lower[i - 1])) {
      if (!isBoundaryChar(lower[i])) {
        wordStarts.push(i);
        acronym += lower[i];
      }
    }
  }
  return { text, lower, wordStarts, acronym };
}

/**
 * Score a lowercased query against a prebuilt `FuzzyEntry`. Higher is
 * better, 0 means no match. Rewards word-start matches, acronym prefix
 * matches, and consecutive runs; shorter names win ties. Pure.
 */
export function fuzzyScoreEntry(queryLower: string, entry: FuzzyEntry): number {
  const q = queryLower.toLowerCase().trim();
  const t = entry.lower;
  if (!q || !t) return 0;
  let score = 0;
  let ti = 0;
  let lastMatch = -2;
  for (let qi = 0; qi < q.length; qi++) {
    const found = t.indexOf(q[qi], ti);
    if (found === -1) return 0;
    if (
      found === 0 ||
      entry.wordStarts.includes(found) ||
      t[found - 1] === " " ||
      t[found - 1] === "-" ||
      t[found - 1] === "_"
    ) {
      score += 3;
    } else if (found === lastMatch + 1) {
      score += 2;
    } else {
      score += 1;
    }
    lastMatch = found;
    ti = found + 1;
  }
  if (entry.acronym.length > 1 && entry.acronym.startsWith(q)) {
    score += 4;
  }
  score += Math.max(0, 20 - t.length);
  return score;
}

/**
 * Subsequence fuzzy score. Higher is better, 0 means no match. Rewards
 * word-start matches and consecutive runs; shorter names win ties. Pure.
 */
export function fuzzyScore(query: string, text: string): number {
  return fuzzyScoreEntry(query, buildFuzzyEntry(text));
}

/**
 * Rank launcher items for the quick-switch palette. Pure: scores each
 * item's title (plus its context line, so "work gmail" finds the Work
 * account), drops non-matches, best first, capped for the palette.
 * Empty query keeps the incoming (browse) order.
 */
export function rankQuickSwitch(
  query: string,
  items: SearchResult[],
  entriesOf?: (it: SearchResult) => FuzzyEntry[]
): SearchResult[] {
  const q = query.trim();
  if (!q) return items.slice(0, 15);
  const ql = q.toLowerCase();
  return items
    .map((it) => {
      let score = 0;
      if (entriesOf) {
        for (const e of entriesOf(it)) {
          const s = fuzzyScoreEntry(ql, e);
          if (s > score) score = s;
        }
      } else {
        const contextScore = it.context
          ? fuzzyScore(q, `${it.title} ${it.context}`)
          : 0;
        score = Math.max(fuzzyScore(q, it.title), contextScore);
      }
      return { it, score };
    })
    .filter((x) => x.score > 0)
    .sort((a, b) => b.score - a.score)
    .slice(0, 15)
    .map((x) => x.it);
}

/** Minimal app descriptor fingerprinted by the persisted search cache. */
export interface SearchCacheApp {
  id: string;
  name: string;
  url: string;
  accounts: Array<{ id: string; label: string }>;
}

export interface SearchCacheRoutine {
  id: string;
  name: string;
}

export interface SearchCachePayload {
  version: number;
  fingerprint: string;
  savedAt: number;
  apps: SearchCacheApp[];
  programs: NativeProgram[];
  routines: SearchCacheRoutine[];
  entries: Record<string, FuzzyEntry[]>;
}

const SEARCH_CACHE_VERSION = 1;

function computeSearchFingerprint(
  apps: SearchCacheApp[],
  programs: NativeProgram[],
  routines: SearchCacheRoutine[]
): string {
  const parts: string[] = [];
  for (const a of apps) {
    const accts = a.accounts.map((ac) => `${ac.id}:${ac.label}`).join(",");
    parts.push(`app:${a.id}:${a.name}:${a.url}:[${accts}]`);
  }
  for (const p of programs) {
    parts.push(`prog:${p.id}:${p.name}:${p.exe_path}`);
  }
  for (const r of routines) {
    parts.push(`rt:${r.id}:${r.name}`);
  }
  return parts.join("|");
}

export function buildSearchCachePayload(
  apps: SearchCacheApp[],
  programs: NativeProgram[],
  routines: SearchCacheRoutine[],
  entries: Record<string, FuzzyEntry[]>,
  savedAt: number
): SearchCachePayload {
  return {
    version: SEARCH_CACHE_VERSION,
    fingerprint: computeSearchFingerprint(apps, programs, routines),
    savedAt,
    apps,
    programs,
    routines,
    entries,
  };
}

function isFuzzyEntry(v: unknown): v is FuzzyEntry {
  if (!v || typeof v !== "object") return false;
  const o = v as Record<string, unknown>;
  return (
    typeof o.text === "string" &&
    typeof o.lower === "string" &&
    Array.isArray(o.wordStarts) &&
    typeof o.acronym === "string"
  );
}

export function parseSearchCachePayload(raw: string): SearchCachePayload | null {
  try {
    const parsed: unknown = JSON.parse(raw);
    if (!parsed || typeof parsed !== "object") return null;
    const o = parsed as Record<string, unknown>;
    if (o.version !== SEARCH_CACHE_VERSION) return null;
    if (typeof o.fingerprint !== "string") return null;
    if (
      !Array.isArray(o.programs) ||
      !Array.isArray(o.apps) ||
      !Array.isArray(o.routines)
    ) {
      return null;
    }
    if (!o.entries || typeof o.entries !== "object" || Array.isArray(o.entries)) {
      return null;
    }
    const entries: Record<string, FuzzyEntry[]> = {};
    for (const [k, v] of Object.entries(o.entries as Record<string, unknown>)) {
      if (Array.isArray(v) && v.every(isFuzzyEntry)) {
        entries[k] = v;
      }
    }
    return {
      version: SEARCH_CACHE_VERSION,
      fingerprint: o.fingerprint,
      savedAt: typeof o.savedAt === "number" ? o.savedAt : 0,
      apps: o.apps as SearchCacheApp[],
      programs: o.programs as NativeProgram[],
      routines: o.routines as SearchCacheRoutine[],
      entries,
    };
  } catch {
    return null;
  }
}
