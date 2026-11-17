import { readFile } from "node:fs/promises";
import { createReadStream } from "node:fs";
import { join } from "node:path";
import { createHash } from "node:crypto";
import coreRelease from "./core-release.json";
import { AppError } from "./errors.ts";
import { parse, record } from "./json.ts";

/** The CLIProxyAPI version this build pins. */
export const coreVersion = coreRelease.version;

/**
 * Checks the core binary against the `manifest.json` beside it and, when `requirePinnedCore`
 * is set, against the binary hash this build pins for the current platform.
 */
export async function verifyCore(
  binary: string,
  requirePinnedCore = false,
): Promise<void> {
  const manifest = record(
    parse(await readFile(join(binary, "..", "manifest.json"), "utf8")),
  );
  const digest = createHash("sha256");
  for await (const chunk of createReadStream(binary)) digest.update(chunk);
  const hash = digest.digest("hex");
  // Headless releases must also match the compiled pin. Desktop signing
  // changes the binary and refreshes its manifest after packaging verifies it.
  const pin = new Map(Object.entries(coreRelease.binaries)).get(
    `${process.platform}_${process.arch}`,
  );
  if (manifest.binarySha256 !== hash || (requirePinnedCore && pin !== hash))
    throw new AppError("The bundled core failed its integrity check.", 503);
}
