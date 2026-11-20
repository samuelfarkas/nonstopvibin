import {
  chmod,
  copyFile,
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  writeFile,
} from "node:fs/promises";
import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import {
  coreLayout,
  coreManifest,
  coreVersion,
  pinned,
  pinnedArchive,
  pinnedBinary,
  writeCoreManifest,
} from "../src/server/core-binary.ts";
import { architecture } from "./distribution.ts";

const platform = process.env.CORE_PLATFORM || process.platform;
const arch = process.env.CORE_ARCH || process.arch;
if (!pinned(platform, arch))
  throw new Error("Supported core platforms: macOS/Linux arm64 and x64.");
const archive = `CLIProxyAPI_${coreVersion}_${platform}_${architecture(arch).core}.tar.gz`;
// Relative to the working directory: scripts/build-server.mjs stages a core in its own.
const root = resolve(coreLayout.checkout);
const temporary = await mkdtemp(join(tmpdir(), "nonstopvibin-core-"));
const extracted = join(temporary, "extracted");
const archivePath = join(temporary, archive);
const origin = `https://github.com/router-for-me/CLIProxyAPI/releases/download/v${coreVersion}`;
async function download(name) {
  const response = await fetch(`${origin}/${name}`, {
    signal: AbortSignal.timeout(60_000),
  });
  if (!response.ok)
    throw new Error(`Download ${name}: HTTP ${response.status}`);
  return Buffer.from(await response.arrayBuffer());
}

try {
  await mkdir(extracted);
  const checksums = (await download("checksums.txt")).toString();
  const expected = checksums
    .split("\n")
    .find((line) => line.trim().endsWith(archive))
    ?.split(/\s+/)[0];
  if (!expected || !/^[a-f0-9]{64}$/i.test(expected))
    throw new Error("Official checksum missing.");
  if (!pinnedArchive(platform, arch, expected))
    throw new Error("Official checksum differs from the reviewed release pin.");
  const bytes = await download(archive);
  const actual = createHash("sha256").update(bytes).digest("hex");
  if (actual !== expected)
    throw new Error("Core checksum mismatch; refusing to install.");
  await writeFile(archivePath, bytes);
  const entries = execFileSync("tar", ["-tzf", archivePath], {
    encoding: "utf8",
  })
    .trim()
    .split("\n");
  if (
    entries.some(
      (entry) => entry.startsWith("/") || entry.split("/").includes(".."),
    )
  )
    throw new Error("Unsafe archive paths.");
  execFileSync("tar", ["-xzf", archivePath, "-C", extracted]);
  for (const file of [coreLayout.binary, "LICENSE"]) {
    const status = await lstat(join(extracted, file));
    if (!status.isFile() || status.isSymbolicLink())
      throw new Error(`Core archive is missing a regular ${file} file.`);
  }
  const binarySha256 = createHash("sha256")
    .update(await readFile(join(extracted, coreLayout.binary)))
    .digest("hex");
  if (!pinnedBinary(platform, arch, binarySha256))
    throw new Error("Core binary differs from the reviewed release pin.");

  await rm(root, { recursive: true, force: true });
  await mkdir(root, { recursive: true });
  await copyFile(
    join(extracted, coreLayout.binary),
    join(root, coreLayout.binary),
  );
  await chmod(join(root, coreLayout.binary), 0o755);
  await copyFile(join(extracted, "LICENSE"), join(root, "LICENSE"));
  await mkdir("licenses", { recursive: true });
  await copyFile(join(extracted, "LICENSE"), "licenses/CLIProxyAPI.txt");
  writeCoreManifest(
    root,
    coreManifest(platform, arch, archive, actual, binarySha256),
  );
  console.log(`Verified CLIProxyAPI ${coreVersion} (${platform}/${arch}).`);
} finally {
  await rm(temporary, { recursive: true, force: true });
}
