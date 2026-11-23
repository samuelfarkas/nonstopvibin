import { copyFile, mkdir, mkdtemp, rename, rm } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { bundledPin, coreLayout, coreTarget } from "./core-binary.ts";
import { installCore } from "./core-install.ts";

const platform = process.env.CORE_PLATFORM || process.platform;
const arch = process.env.CORE_ARCH || process.arch;
if (!coreTarget(platform, arch))
  throw new Error("Supported core platforms: macOS/Linux arm64 and x64.");
// Relative to the working directory: scripts/build-server.mjs stages a core in its own.
const root = resolve(coreLayout.checkout);
await mkdir(dirname(root), { recursive: true });
// Beside the core it replaces, so one rename swaps in a verified one.
const staging = await mkdtemp(`${root}-`);
try {
  await installCore(bundledPin, platform, arch, staging, { license: true });
  await mkdir("licenses", { recursive: true });
  await copyFile(join(staging, "LICENSE"), "licenses/CLIProxyAPI.txt");
  await rm(root, { recursive: true, force: true });
  await rename(staging, root);
  console.log(
    `Verified CLIProxyAPI ${bundledPin.version} (${platform}/${arch}).`,
  );
} finally {
  await rm(staging, { recursive: true, force: true });
}
