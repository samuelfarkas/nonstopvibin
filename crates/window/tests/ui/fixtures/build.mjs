// Builds the UI checks' fixture pages into dist/ui-fixtures with the app's own Vite config
// (plugins and build target), so they run the client code as the app ships it.
// Run from anywhere: `bun crates/window/tests/ui/fixtures/build.mjs`.
import { fileURLToPath } from "node:url";
import { build } from "vite";

const here = (path) => fileURLToPath(new URL(path, import.meta.url));
const checkout = here("../../../../../");

await build({
  configFile: `${checkout}vite.config.ts`,
  root: here("."),
  publicDir: `${checkout}public`,
  logLevel: "warn",
  build: {
    outDir: `${checkout}dist/ui-fixtures`,
    emptyOutDir: true,
    rollupOptions: {
      input: { focus: here("focus.html"), logo: here("logo.html") },
    },
  },
});
