import { defineConfig } from "cf/config";

export default defineConfig({
  worker: {
    name: "capturefab-site",
    compatibilityDate: "2026-10-01",
    assets: { htmlHandling: "auto-trailing-slash", notFoundHandling: "none" },
    domains: ["capturefab.com"],
  },
});
