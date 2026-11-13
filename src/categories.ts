// Auto-sections for the launcher: every tile lands in exactly one
// section (AI, Games, Media, Music, Other) by its name and URL, so the
// launcher can filter to one section at a time. No store change: the
// category is computed from the app name + URL at render time, and an
// app that matches nothing lands in Other. Keep the lists to services
// the user actually uses; a wrong guess is cheap because the tile is
// still one click away under All.
import type { SearchResult } from "./Launcher";

export type AppCategory = "ai" | "games" | "media" | "music" | "other";

export type Section = "all" | AppCategory;

export const SECTION_LABELS: Record<Section, string> = {
  all: "All",
  ai: "AI",
  games: "Games",
  media: "Media",
  music: "Music",
  other: "Other",
};

/** Sections in chip order. */
export const SECTIONS: Section[] = ["all", "ai", "games", "media", "music", "other"];

// Substring rules, matched against the lowercased "name url" text.
// Order matters: Music runs before Media so "YouTube Music" lands in
// Music while plain "YouTube" lands in Media.
const RULES: Array<{ category: AppCategory; keywords: string[] }> = [
  {
    category: "ai",
    keywords: [
      "chatgpt",
      "openai",
      "claude",
      "anthropic",
      "Muse",
      "gemini",
      "copilot",
      "perplexity",
      "grok",
      "x.ai",
      "llama",
      "mistral",
      "deepseek",
      "huggingface",
      "hf.co",
      "character.ai",
      "c.ai",
      "poe.com",
      "midjourney",
      "dall-e",
      "dalle",
      "leonardo",
      "runway",
      "replicate",
      "together.ai",
      "groq",
      "ollama",
      "notebooklm",
      "cursor",
      "windsurf",
      "v0.dev",
      "bolt.new",
      "lovable",
      "replit",
      "codeium",
      "aistudio",
    ],
  },
  {
    category: "games",
    keywords: [
      "steam",
      "steampowered",
      "epicgames",
      "epic games",
      "xbox",
      "playstation",
      "psn",
      "nintendo",
      "roblox",
      "minecraft",
      "itch.io",
      "gog.com",
      "battle.net",
      "blizzard",
      "riot games",
      "league of legends",
      "valorant",
      "fortnite",
      "ubisoft",
      "rockstar",
      "geforcenow",
      "xcloud",
      "game pass",
      "discord",
    ],
  },
  {
    category: "music",
    keywords: [
      "spotify",
      "apple music",
      "music.apple",
      "youtube music",
      "music.youtube",
      "soundcloud",
      "tidal",
      "deezer",
      "pandora",
      "amazon music",
      "music.amazon",
      "iheartradio",
      "tunein",
      "bandcamp",
      "last.fm",
      "lastfm",
    ],
  },
  {
    category: "media",
    keywords: [
      "youtube",
      "youtu.be",
      "netflix",
      "hulu",
      "disney+",
      "disneyplus",
      "prime video",
      "amazon video",
      "hbomax",
      "hbo",
      "max.com",
      "peacock",
      "paramount",
      "apple tv",
      "appletv",
      "twitch",
      "vimeo",
      "dailymotion",
      "plex",
      "jellyfin",
      "emby",
      "stremio",
      "cinehd",
      "crunchyroll",
      "funimation",
      "hianime",
      "tubi",
      "pluto",
      "roku",
      "letterboxd",
      "imdb",
      "trakt",
    ],
  },
];

/** Category for one app from its display name and URL. */
export function categorizeApp(name: string, url?: string): AppCategory {
  const haystack = `${name} ${url ?? ""}`.toLowerCase();
  for (const rule of RULES) {
    if (rule.keywords.some((k) => haystack.includes(k))) return rule.category;
  }
  return "other";
}

/** Category for one launcher tile. Apps and accounts use the app;
 * programs use the program name; routines, searches and workspaces
 * are AppMaka constructs, so they sit in Other. */
export function categoryOfResult(r: SearchResult): AppCategory {
  if (r.kind === "account" || r.kind === "app") {
    return categorizeApp(r.app.name, r.app.url);
  }
  if (r.kind === "program") return categorizeApp(r.program.name);
  return "other";
}
