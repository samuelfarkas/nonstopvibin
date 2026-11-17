const { readFileSync } = require("node:fs");
const { createHash } = require("node:crypto");
const { join } = require("node:path");
const { Arch } = require("electron-builder");
const release = require("../src/server/core-release.json");

module.exports = async function validateCore(context) {
  const root = join(context.packager.projectDir, ".vendor/core");
  const manifest = JSON.parse(
    readFileSync(join(root, "manifest.json"), "utf8"),
  );
  const arch = Arch[context.arch];
  if (
    manifest.version !== release.version ||
    manifest.sha256 !==
      release.checksums[`${manifest.platform}_${manifest.arch}`]
  )
    throw new Error("The proxy core does not match the reviewed release pin.");
  if (
    manifest.platform !== context.electronPlatformName ||
    manifest.arch !== arch
  )
    throw new Error(
      `The proxy core is ${manifest.platform}/${manifest.arch}, but the app target is ${context.electronPlatformName}/${arch}. Run core:install for the target first.`,
    );
  const binaryPin = release.binaries[`${manifest.platform}_${manifest.arch}`];
  if (!binaryPin)
    throw new Error("The proxy core target has no reviewed binary pin.");
  if (manifest.binarySha256 !== binaryPin)
    throw new Error(
      "The proxy core manifest does not match the reviewed binary pin. Run bun run core:install again.",
    );
  const digest = createHash("sha256")
    .update(readFileSync(join(root, "cli-proxy-api")))
    .digest("hex");
  if (digest !== binaryPin)
    throw new Error(
      "The proxy core binary does not match the reviewed release pin. Run bun run core:install again.",
    );
};
