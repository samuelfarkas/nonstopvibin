// `bun run dev`: the Rust daemon from this checkout in development mode, which accepts the
// Vite server's Host and Origin, and Vite with hot reload proxying /api to it. Open the link
// the daemon prints.
import { spawn } from "node:child_process";
const port = process.env.NONSTOPVIBIN_PORT || "4318";
const children = [
  spawn(
    "cargo",
    ["run", "--quiet", "--bin", "nonstopvibin", "--", "serve", "--port", port],
    {
      stdio: "inherit",
      env: { ...process.env, NONSTOPVIBIN_DEVELOPMENT: "1" },
    },
  ),
  spawn(process.execPath, ["run", "vite"], { stdio: "inherit" }),
];
let stopping = false;
function stop() {
  if (stopping) return;
  stopping = true;
  for (const child of children) child.kill("SIGTERM");
}
process.on("SIGINT", stop);
process.on("SIGTERM", stop);
for (const child of children)
  child.on("exit", (code) => {
    if (!stopping) {
      process.exitCode = code ?? 1;
      stop();
    }
  });
