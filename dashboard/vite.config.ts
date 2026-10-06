import { fileURLToPath } from "node:url";

import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import type { ProxyOptions } from "vite";
import { defineConfig } from "vitest/config";

import { PROXIED_PREFIXES, devProxyTarget } from "./build/devTarget.ts";
import { thirdPartyLicenses } from "./build/licenses.ts";
import { SECURITY_HEADERS } from "./build/securityHeaders.ts";

const root = fileURLToPath(new URL(".", import.meta.url));

/** The API proxy of the development servers, to OPEN_FERRY_URL. */
function apiProxy(): Record<string, ProxyOptions> {
  const target = devProxyTarget(process.env.OPEN_FERRY_URL);
  return Object.fromEntries(
    PROXIED_PREFIXES.map((prefix) => [prefix, { target, changeOrigin: true, ws: false }]),
  );
}

export default defineConfig(({ command }) => {
  // `vite` and `vite preview` proxy the API; `vite build` and Vitest don't.
  const serving = command === "serve" && process.env.VITEST === undefined;
  const proxy = serving ? apiProxy() : undefined;
  return {
    // open-ferry serves the app under /dashboard/.
    base: "/dashboard/",
    plugins: [
      react(),
      tailwindcss(),
      thirdPartyLicenses({ root, include: ["tailwindcss"] }),
    ],
    build: {
      outDir: "dist",
      assetsDir: "assets",
      emptyOutDir: true,
      sourcemap: false,
      // Everything is a file of its own: the Content-Security-Policy allows
      // no data: fonts, and small files cost nothing served from the binary.
      assetsInlineLimit: 0,
      modulePreload: { polyfill: false },
      chunkSizeWarningLimit: 1024,
    },
    server: {
      host: "127.0.0.1",
      port: 5173,
      strictPort: true,
      proxy,
    },
    preview: {
      host: "127.0.0.1",
      port: 4173,
      strictPort: true,
      proxy,
      headers: { ...SECURITY_HEADERS },
    },
    test: {
      environment: "jsdom",
      include: ["src/**/*.test.{ts,tsx}", "build/**/*.test.ts"],
      setupFiles: ["src/test/setup.ts"],
      restoreMocks: true,
      unstubGlobals: true,
      css: false,
      // Times show in the browser's zone; tests pin it.
      env: { TZ: "UTC" },
    },
  };
});
