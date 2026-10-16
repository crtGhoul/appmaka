/**
 * v0.15.0: Ctrl+K quick-switch. The fuzzy matcher lives here as its own
 * pure module (the launcher search imports it from here too, so there is
 * exactly one matcher); `rankQuickSwitch` is the palette's ranking seam.
 */
import type { SearchResult } from "./Launcher";

/**
 * Subsequence fuzzy score. Higher is better, 0 means no match. Rewards
 * word-start matches and consecutive runs; shorter names win ties. Pure.
 */
export function fuzzyScore(query: string, text: string): number {
  const q = query.toLowerCase().trim();
  const t = text.toLowerCase();
  if (!q) return 0;
  let score = 0;
  let ti = 0;
  let lastMatch = -2;
  for (let qi = 0; qi < q.length; qi++) {
    const found = t.indexOf(q[qi], ti);
    if (found === -1) return 0;
    if (found === 0 || t[found - 1] === " " || t[found - 1] === "-" || t[found - 1] === "_") {
      score += 3;
    } else if (found === lastMatch + 1) {
      score += 2;
    } else {
      score += 1;
    }
    lastMatch = found;
    ti = found + 1;
  }
  score += Math.max(0, 20 - t.length);
  return score;
}

/**
 * Rank launcher items for the quick-switch palette. Pure: scores each
 * item's title (plus its context line, so "work gmail" finds the Work
 * account), drops non-matches, best first, capped for the palette.
 * Empty query keeps the incoming (browse) order.
 */
export function rankQuickSwitch(
  query: string,
  items: SearchResult[]
): SearchResult[] {
  const q = query.trim();
  if (!q) return items.slice(0, 15);
  return items
    .map((it) => {
      const contextScore = it.context
        ? fuzzyScore(q, `${it.title} ${it.context}`)
        : 0;
      return { it, score: Math.max(fuzzyScore(q, it.title), contextScore) };
    })
    .filter((x) => x.score > 0)
    .sort((a, b) => b.score - a.score)
    .slice(0, 15)
    .map((x) => x.it);
}
