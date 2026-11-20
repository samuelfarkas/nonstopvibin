import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import {
  checkoutLayout,
  devServer,
  hostPort,
} from "./src/server/host-defaults.ts";

export default defineConfig({
  plugins: [react()],
  server: {
    ...devServer,
    strictPort: true,
    proxy: {
      "/api": `http://127.0.0.1:${hostPort()}`,
    },
  },
  build: {
    target: "chrome152",
    outDir: checkoutLayout.client,
    sourcemap: false,
  },
});
