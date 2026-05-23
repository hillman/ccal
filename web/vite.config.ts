import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import wasm from "vite-plugin-wasm";
import topLevelAwait from "vite-plugin-top-level-await";
import { VitePWA } from "vite-plugin-pwa";

// ccal-server is reached same-origin: in dev, vite proxies /sync (WebSocket)
// and /mcp to the running server (default 127.0.0.1:8787, override with
// CCAL_SERVER); in production the built app is served BY ccal-server, so the
// same relative paths resolve to it directly. No CORS, no separate host.
const server = process.env.CCAL_SERVER ?? "127.0.0.1:8787";

export default defineConfig({
  plugins: [
    // @automerge/automerge ships WASM; these two plugins let vite bundle it.
    wasm(),
    topLevelAwait(),
    react(),
    // Installable, offline-capable PWA: precache the app shell (incl. the
    // WASM) and auto-update. The SW must never intercept the sync socket or
    // the MCP endpoint — only the static shell.
    VitePWA({
      registerType: "autoUpdate",
      includeAssets: ["icon.svg"],
      manifest: {
        name: "ccal",
        short_name: "ccal",
        description: "ccal notes & todos",
        theme_color: "#1e1e2e",
        background_color: "#1e1e2e",
        display: "standalone",
        start_url: "/",
        icons: [
          { src: "/icon.svg", sizes: "any", type: "image/svg+xml", purpose: "any maskable" },
        ],
      },
      workbox: {
        globPatterns: ["**/*.{js,css,html,svg,wasm}"],
        maximumFileSizeToCacheInBytes: 4 * 1024 * 1024, // automerge WASM ~1.9MB
        navigateFallback: "/index.html",
        navigateFallbackDenylist: [/^\/sync/, /^\/mcp/],
      },
    }),
  ],
  optimizeDeps: {
    // The WASM glue must not be pre-bundled by esbuild.
    exclude: ["@automerge/automerge", "@automerge/automerge-wasm"],
  },
  server: {
    proxy: {
      "/sync": { target: `ws://${server}`, ws: true, changeOrigin: true },
      "/mcp": { target: `http://${server}`, changeOrigin: true },
    },
  },
});
