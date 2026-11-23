// `bun scripts/sign-release.ts <release-dir>`: signs <release-dir>/SHA256SUMS with minisign
// into SHA256SUMS.minisig, which the Rust updater verifies with the public key in
// build/update-signing.pub. The secret key file's text comes from MINISIGN_SECRET_KEY and its
// password, if it has one, from MINISIGN_PASSWORD (the release environment's secrets).
// Without the secret it warns and signs nothing, until the public key is committed: from then
// on installed builds expect a signature, so a release without one fails here.
import { execFileSync } from "node:child_process";
import { existsSync } from "node:fs";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import pkg from "../package.json" with { type: "json" };
import { releaseTool } from "./release-tools.ts";

export const publicKey = resolve(
  import.meta.dirname,
  "../build/update-signing.pub",
);

export async function signRelease(
  directory: string,
  secretKey = process.env.MINISIGN_SECRET_KEY,
  password = process.env.MINISIGN_PASSWORD ?? "",
  minisign?: string,
): Promise<boolean> {
  const sums = join(directory, "SHA256SUMS");
  if (!existsSync(sums)) throw new Error(`Missing ${sums}`);
  if (!secretKey) {
    if (existsSync(publicKey))
      throw new Error(
        "build/update-signing.pub is committed, so releases must be signed: add MINISIGN_SECRET_KEY to the release environment.",
      );
    console.log(
      "::warning::SHA256SUMS is not signed: the release environment has no MINISIGN_SECRET_KEY.",
    );
    return false;
  }
  const tool = minisign ?? (await releaseTool("minisign"));
  const work = await mkdtemp(join(tmpdir(), "nv-minisign-"));
  try {
    const key = join(work, "release.key");
    await writeFile(
      key,
      secretKey.endsWith("\n") ? secretKey : `${secretKey}\n`,
      { mode: 0o600 },
    );
    const signature = `${sums}.minisig`;
    execFileSync(
      tool,
      [
        "-S",
        "-s",
        key,
        "-m",
        sums,
        "-x",
        signature,
        "-t",
        `NonstopVibin ${pkg.version} SHA256SUMS`,
      ],
      // minisign reads the password from stdin when it is not a terminal.
      { input: `${password}\n`, stdio: ["pipe", "ignore", "inherit"] },
    );
    if (existsSync(publicKey))
      execFileSync(
        tool,
        ["-V", "-q", "-p", publicKey, "-m", sums, "-x", signature],
        {
          stdio: "inherit",
        },
      );
    console.log(`Signed ${sums}.`);
    return true;
  } finally {
    await rm(work, { recursive: true, force: true });
  }
}

if (import.meta.main) {
  const [directory] = process.argv.slice(2);
  if (!directory)
    throw new Error("Usage: bun scripts/sign-release.ts <release-dir>");
  await signRelease(directory);
}
