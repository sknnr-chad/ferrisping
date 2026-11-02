import { defineConfig } from "cf/config";

// A Worker with static assets and no code of its own: it serves public/ (named in wrangler.config.ts).
// The account comes from CLOUDFLARE_ACCOUNT_ID.
export default defineConfig({
  worker: {
    name: "ferrisping-site",
    compatibilityDate: "2026-10-19",
    assets: {
      htmlHandling: "auto-trailing-slash",
      notFoundHandling: "404-page",
    },
    domains: ["ferrisping.com"],
    // Served only at ferrisping.com: no workers.dev address and no preview URLs.
    workersDev: false,
    previewUrls: false,
    // There's no Worker code, so nothing to log.
    observability: { enabled: false },
  },
});
