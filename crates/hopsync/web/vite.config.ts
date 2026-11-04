// Build settings for the UI that `build.rs` compiles into the binary.
//
// - No inlined assets: the CSP refuses `data:` URLs, so fonts stay separate files.
// - No source maps: `build.rs` refuses file types it does not expect, and they would only add size.
// - `.gz` copies are made here so the daemon never compresses at runtime.

import { readdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { gzipSync } from "node:zlib";
import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig, type Plugin } from "vite";

function gzipOutput(): Plugin {
  const visit = (dir: string) => {
    for (const name of readdirSync(dir)) {
      const path = join(dir, name);
      if (statSync(path).isDirectory()) visit(path);
      else if (/\.(js|css)$/.test(name)) {
        writeFileSync(`${path}.gz`, gzipSync(readFileSync(path), { level: 9 }));
      }
    }
  };
  return {
    name: "hopsync-gzip",
    apply: "build",
    closeBundle: () => visit("dist"),
  };
}

// `pnpm dev` talks to a daemon on the default port. The daemon's CSRF check compares Origin with
// Host, so the proxy rewrites both.
const daemon = process.env.HOPSYNC_DAEMON ?? "http://127.0.0.1:7890";

export default defineConfig({
  plugins: [react(), tailwindcss(), gzipOutput()],
  build: {
    assetsInlineLimit: 0,
    sourcemap: false,
    modulePreload: { polyfill: false },
    chunkSizeWarningLimit: 700,
  },
  server: {
    proxy: {
      "/api": {
        target: daemon,
        changeOrigin: true,
        configure: (proxy) => {
          proxy.on("proxyReq", (req) => req.setHeader("Origin", daemon));
        },
      },
    },
  },
  test: {
    environment: "jsdom",
    include: ["src/**/*.test.{ts,tsx}"],
  },
} as Parameters<typeof defineConfig>[0]);
