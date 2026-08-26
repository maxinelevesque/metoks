import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Builds into ../static, which the Rust backend serves at `/`.
// In dev, `npm run dev` proxies /api to the running backend on :8787.
export default defineConfig({
  plugins: [react()],
  build: {
    outDir: "../static",
    emptyOutDir: true,
  },
  server: {
    proxy: {
      "/api": {
        target: "http://127.0.0.1:8787",
        changeOrigin: true,
      },
    },
  },
});
