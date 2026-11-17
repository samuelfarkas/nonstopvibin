const { execFileSync } = require("node:child_process");
const { createHash } = require("node:crypto");
const { readFileSync, writeFileSync } = require("node:fs");
const { join } = require("node:path");

// electron-builder calls this instead of @electron/osx-sign when mac.sign is set.
// Signing rewrites the core binary, so its manifest hash is refreshed here,
// before the app seal is created, and the core is skipped in the main pass.
// The core and tray helper are not Electron processes, so they get hardened
// runtime without the JIT/executable-memory exceptions Electron needs.
const minimal = join(__dirname, "../build/entitlements.mac.minimal.plist");

module.exports = async function signMac(opts) {
  const { signAsync } = require("@electron/osx-sign");
  const coreDir = join(opts.app, "Contents/Resources/core");
  const core = join(coreDir, "cli-proxy-api");
  const helper = join(
    opts.app,
    "Contents/Resources/app.asar.unpacked/dist/desktop/tray-click-monitor",
  );
  const perFile = await opts.optionsForFile(core);

  const args = ["--sign", opts.identity, "--force"];
  if (opts.keychain) args.push("--keychain", opts.keychain);
  if (perFile.timestamp) args.push(`--timestamp=${perFile.timestamp}`);
  else args.push("--timestamp");
  if (perFile.requirements) {
    if (perFile.requirements.startsWith("="))
      args.push(`-r${perFile.requirements}`);
    else args.push("--requirements", perFile.requirements);
  }
  if (perFile.hardenedRuntime) args.push("--options", "runtime");
  if (perFile.additionalArguments) args.push(...perFile.additionalArguments);
  args.push("--entitlements", minimal, core);
  execFileSync("codesign", args, { stdio: "inherit" });

  const manifestPath = join(coreDir, "manifest.json");
  const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
  manifest.binarySha256 = createHash("sha256")
    .update(readFileSync(core))
    .digest("hex");
  writeFileSync(manifestPath, JSON.stringify(manifest, null, 2));

  const ignore = opts.ignore;
  const optionsForFile = opts.optionsForFile;
  await signAsync({
    ...opts,
    optionsForFile: (file) => {
      const options = optionsForFile(file);
      return file === helper ? { ...options, entitlements: minimal } : options;
    },
    ignore: (file) =>
      file === core || (typeof ignore === "function" ? ignore(file) : false),
  });
};
