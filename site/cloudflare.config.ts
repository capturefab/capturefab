import { bindings, defineConfig } from "cf/config";

export default defineConfig({
  worker: {
    name: "capturefab-site",
    compatibilityDate: "2026-10-01",
    // Only /download/* runs the Worker; every other path is a static asset from web/.
    entrypoint: "./src/worker.ts",
    assets: { htmlHandling: "auto-trailing-slash", notFoundHandling: "none", runWorkerFirst: ["/download/*"] },
    domains: ["capturefab.com"],
    env: {
      // Release files, uploaded by scripts/publish-downloads.py as v<version>/<file>.
      DOWNLOADS: bindings.r2({ name: "capturefab-downloads" }),
    },
  },
});
