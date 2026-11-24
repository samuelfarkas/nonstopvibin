// Downloads that nothing uses before their reviewed SHA-256 matches, and the two digests the
// release files carry. scripts/release-tools.ts and scripts/install-security-tools.mjs pin their
// tools with it.
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

/** Hex SHA-256: the pins, SHA256SUMS and the packaging report. */
export const sha256 = (bytes: Uint8Array): string =>
  createHash("sha256").update(bytes).digest("hex");
/** Base64 SHA-512: the entries of electron-updater's latest-*.yml. */
export const sha512 = (bytes: Uint8Array): string =>
  createHash("sha512").update(bytes).digest("base64");

export interface Pin {
  url: string;
  sha256: string;
  /** The file inside a .tar.gz download, and its own SHA-256 when one was reviewed. */
  member?: { path: string; sha256?: string };
}

/** The pinned file, or for an archive its member, after every reviewed hash matched. */
export async function pinnedDownload(
  name: string,
  pin: Pin,
  timeout: number,
): Promise<Buffer> {
  const response = await fetch(pin.url, {
    signal: AbortSignal.timeout(timeout),
  });
  if (!response.ok)
    throw new Error(`${name} download: HTTP ${response.status}`);
  const bytes = Buffer.from(await response.arrayBuffer());
  const mismatch = new Error(`${name} checksum mismatch; refusing to install.`);
  if (sha256(bytes) !== pin.sha256) throw mismatch;
  if (!pin.member) return bytes;
  const temp = await mkdtemp(join(tmpdir(), "nonstopvibin-download-"));
  try {
    const archive = join(temp, "download.tar.gz");
    await writeFile(archive, bytes);
    // GNU tar warns about the macOS xattr headers some upstream tarballs carry; bsdtar has no
    // such option.
    const quiet =
      process.platform === "linux" ? ["--warning=no-unknown-keyword"] : [];
    execFileSync("tar", [
      ...quiet,
      "-xzf",
      archive,
      "-C",
      temp,
      pin.member.path,
    ]);
    const member = await readFile(join(temp, pin.member.path));
    if (pin.member.sha256 && sha256(member) !== pin.member.sha256)
      throw mismatch;
    return member;
  } finally {
    await rm(temp, { recursive: true, force: true });
  }
}
