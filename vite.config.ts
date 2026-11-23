import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { clientDirectory } from "./scripts/distribution.ts";

export default defineConfig({
  plugins: [react()],
  server: {
    // A development daemon accepts this Host and Origin (DEV_HOST in crates/server/src/http.rs).
    host: "127.0.0.1",
    port: 5173,
    strictPort: true,
    proxy: {
      "/api": `http://127.0.0.1:${process.env.NONSTOPVIBIN_PORT || 4318}`,
    },
  },
  build: {
    // macOS 13 runs the window in Safari 16's WebKit; browsers reach the headless server.
    target: ["chrome152", "safari16"],
    outDir: clientDirectory,
    sourcemap: false,
  },
});
