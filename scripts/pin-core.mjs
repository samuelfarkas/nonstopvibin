// `bun run core:pin [version]`: writes src/server/core-release.json for an upstream
// CLIProxyAPI release, the latest without a version. Review the diff before committing it:
// installed apps offer a pin as soon as it reaches main.
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  coreArchive,
  coreLayout,
  corePinSchema,
  coreTargets,
} from "../src/server/core-binary.ts";
import { coreArchives, download, extract } from "../src/server/core-install.ts";

const releases = "https://github.com/router-for-me/CLIProxyAPI/releases";
const pinPath = join(import.meta.dirname, "../src/server/core-release.json");

async function latest() {
  const response = await fetch(`${releases}/latest`, {
    redirect: "manual",
    signal: AbortSignal.timeout(30_000),
  });
  const location = response.headers.get("location") ?? "";
  const tag = /\/releases\/tag\/v([^/]+)$/.exec(location)?.[1];
  if (!tag) throw new Error(`No latest release tag (HTTP ${response.status}).`);
  return tag;
}

const version = process.argv[2] ?? (await latest());
if (!/^\d+\.\d+\.\d+$/.test(version))
  throw new Error(`Not a CLIProxyAPI version: ${version}`);
const origin = `${coreArchives}/v${version}`;
const temporary = await mkdtemp(join(tmpdir(), "nonstopvibin-pin-"));
try {
  const listing = join(temporary, "checksums.txt");
  await download(`${origin}/checksums.txt`, listing);
  // `<sha256>  <archive>` per line.
  const published = new Map(
    (await readFile(listing, "utf8"))
      .split("\n")
      .map((line) => line.trim().split(/\s+/))
      .filter((fields) => fields.length === 2)
      .map(([sha256, name]) => [name, sha256]),
  );
  const checksums = {};
  const binaries = {};
  for (const target of coreTargets) {
    const [platform, arch] = target.split("_");
    const name = coreArchive(version, platform, arch);
    const expected = published.get(name);
    if (!expected)
      throw new Error(
        `CLIProxyAPI ${version}: release still publishing or missing ${target}.`,
      );
    const archive = join(temporary, name);
    const binary = join(temporary, `${target}-${coreLayout.binary}`);
    const sha256 = await download(`${origin}/${name}`, archive);
    if (sha256 !== expected)
      throw new Error(`${name} does not match the release's checksums.txt.`);
    checksums[target] = sha256;
    binaries[target] = await extract(archive, coreLayout.binary, binary, 0o600);
    await rm(archive);
    await rm(binary);
    console.log(`Hashed ${name}.`);
  }
  const { minApp } = corePinSchema.parse(
    JSON.parse(await readFile(pinPath, "utf8")),
  );
  // The schema's key order is the file's.
  const pin = corePinSchema.parse({ version, minApp, checksums, binaries });
  await writeFile(pinPath, `${JSON.stringify(pin, null, 2)}\n`);
  console.log(
    `Pinned CLIProxyAPI ${version}${minApp ? ` for NonstopVibin ${minApp} or later` : ""}. Review src/server/core-release.json.`,
  );
} finally {
  await rm(temporary, { recursive: true, force: true });
}
