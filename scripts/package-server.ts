// Builds release/nonstopvibin-server-<version>-linux-<arch>.tar.gz from the Rust daemon.
// Usage: bun scripts/package-server.ts [x64|arm64 ...] (default: the host architecture).
// Run `bun run build` first. Each architecture needs its Rust musl target and a C compiler
// for it (musl-tools on a native runner); CI builds each on its own runner.
import {
  chmod,
  copyFile,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import pkg from "../package.json" with { type: "json" };
import { coreLayout, coreVersion } from "../src/server/core-binary.ts";
import {
  renderInstaller,
  rustServerArchive,
  rustTarget,
  serverArchiveName,
  serverArchitectures,
} from "./distribution.ts";
import {
  cargoBuild,
  releaseDir,
  repo,
  report,
  run,
  stageCore,
  stageLicences,
  version,
} from "./package-common.ts";
import { verifyServerTree } from "./verify-rust-package.ts";

const arches = process.argv.slice(2).length
  ? process.argv.slice(2)
  : [process.arch];
for (const arch of arches)
  if (!serverArchitectures.includes(arch))
    throw new Error(`Unsupported server architecture: ${arch}`);

await mkdir(releaseDir, { recursive: true });
for (const arch of arches) {
  const build = {
    target: rustTarget("linuxDaemon", arch),
    packages: ["nsv-cli"],
  };
  const binaries = cargoBuild(build);
  const name = serverArchiveName(version, arch);
  const work = await mkdtemp(join(tmpdir(), "nv-server-"));
  try {
    const out = join(work, name);
    const entries = rustServerArchive;
    await mkdir(join(work, "core-stage"));
    await stageCore(
      join(work, "core-stage"),
      join(out, entries.core),
      "linux",
      arch,
    );
    await copyFile(
      join(binaries, "nonstopvibin"),
      join(out, entries.executable),
    );
    await stageLicences(join(out, entries.licenses), [build]);
    await writeFile(
      join(out, entries.installer),
      renderInstaller(
        await readFile(join(repo, "scripts/server-install.sh"), "utf8"),
        version,
        arch,
        Object.values(entries),
      ),
    );
    await writeFile(
      join(out, entries.readme),
      `# NonstopVibin ${version} headless server (linux-${arch})

Run on glibc-based Linux (x64 or arm64; not Alpine/musl) as a non-root user. The
\`nonstopvibin\` executable is static; the bundled CLIProxyAPI core needs glibc 2.17
or later. For trusted co-located agents, use their user. For a protected upstream
consumed through a separate private inference gate, use a dedicated protected
service user with no coding agents; never share its credentials/helpers with guests.
A systemd user session is optional:

    ./install.sh            # ~/.local, plus a systemd user service
    ./install.sh --no-service  # foreground / existing supervisor
    ./install.sh --help     # options

Then run \`nonstopvibin url\` for the session link and SSH tunnel command.
Full guide: ${pkg.homepage}/blob/main/docs/server.md

\`${entries.core}/${coreLayout.manifest}\` records the bundled CLIProxyAPI ${coreVersion} checksums.
They are integrity evidence, not independent proof of the publisher.
Licenses are in \`licenses/\`.
`,
    );
    for (const file of [
      entries.executable,
      entries.installer,
      join(entries.core, coreLayout.binary),
    ])
      await chmod(join(out, file), 0o755);
    verifyServerTree(out, arch);

    const archive = join(releaseDir, `${name}.tar.gz`);
    await rm(archive, { force: true });
    run("tar", [
      "-czf",
      archive,
      "--owner=0",
      "--group=0",
      "--numeric-owner",
      "-C",
      work,
      name,
    ]);
    await report(archive);
  } finally {
    await rm(work, { recursive: true, force: true });
  }
}
