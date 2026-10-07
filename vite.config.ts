import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// @ts-expect-error process is a nodejs global
const host = process.env.TAURI_DEV_HOST;

// https://vite.dev/config/
export default defineConfig(async () => ({
  plugins: [react()],

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  // 2. the preview control window is a second page: its script is bundled
  //    (it needs @tauri-apps/api, which plain public/ files can't import).
  //    The entry HTML sits at the project ROOT so Vite emits it as
  //    dist/preview-header.html — the backend loads
  //    WebviewUrl::App("preview-header.html"). (A src/ entry would be
  //    mirrored to dist/src/preview-header.html, which the backend would
  //    not find.) The clipboard history popup (v0.9.0) follows the same
  //    pattern: clipboard.html -> dist/clipboard.html.
  build: {
    rollupOptions: {
      input: {
        main: "index.html",
        "preview-header": "preview-header.html",
        clipboard: "clipboard.html",
        // v0.10.0: Linux tab-strip window (Windows paints a native strip).
        tabstrip: "tabstrip.html",
      },
    },
  },
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // 3. tell Vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
  },
}));
