// The licence texts of every crate compiled into the shipped Rust binaries, for
// licenses/THIRD-PARTY-RUST.txt. The standard library's own notices (including musl, which
// the static Linux daemon links) come from the toolchain's COPYRIGHT-library.html.
// `bun scripts/rust-licenses.ts <target> <package>...` prints the text for one build.
import { execFileSync } from "node:child_process";
import { readFileSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";

interface Package {
  id: string;
  name: string;
  version: string;
  license: string | null;
  license_file: string | null;
  manifest_path: string;
  repository: string | null;
  source: string | null;
}
interface Node {
  id: string;
  deps: { pkg: string; dep_kinds: { kind: string | null }[] }[];
}

const licenceFile = /^(?:licen[cs]e|copying|copyright|notice|unlicense)/i;

export interface RustBuild {
  target: string;
  /** Workspace packages built for `target`. */
  packages: string[];
}

/** Every non-dev dependency of each build's packages, built for its target. */
export function rustLicenseText(builds: RustBuild[]): string {
  const shipped = new Map<string, Package>();
  for (const build of builds)
    for (const pkg of dependencies(build)) shipped.set(pkg.id, pkg);
  const sections: string[] = [];
  const missing: string[] = [];
  const sorted = [...shipped.values()].sort((a, b) =>
    a.name === b.name
      ? a.version.localeCompare(b.version)
      : a.name.localeCompare(b.name),
  );
  for (const pkg of sorted) {
    const folder = dirname(pkg.manifest_path);
    const files = readdirSync(folder)
      .filter((name) => licenceFile.test(name))
      .sort();
    if (pkg.license_file && !files.includes(pkg.license_file))
      files.push(pkg.license_file);
    const texts = files.map(
      (name) => `--- ${name}\n\n${readFileSync(join(folder, name), "utf8")}`,
    );
    if (!texts.length) missing.push(`${pkg.name} ${pkg.version}`);
    sections.push(
      [
        `${pkg.name} ${pkg.version}`,
        `License: ${pkg.license ?? "see license file"}`,
        ...(pkg.repository ? [`Source: ${pkg.repository}`] : []),
        "",
        ...(texts.length
          ? texts
          : ["The published crate includes no licence file."]),
      ].join("\n"),
    );
  }
  if (missing.length)
    console.warn(
      `No licence file in the published crate, licence expression only: ${missing.join(", ")}`,
    );
  const targets = builds.map((build) => build.target).join(", ");
  return `Third-party Rust crates compiled into NonstopVibin (${targets})\n\n${sections.join(`\n\n${"=".repeat(78)}\n\n`)}\n`;
}

/** The registry crates `packages` depend on for `target`, without dev-dependencies. */
function dependencies({ target, packages: roots }: RustBuild): Package[] {
  // SAFETY: `cargo metadata --format-version 1` has this shape; a change of format fails
  // below on the missing fields.
  const metadata = JSON.parse(
    execFileSync(
      "cargo",
      [
        "metadata",
        "--format-version",
        "1",
        "--locked",
        "--filter-platform",
        target,
      ],
      { encoding: "utf8", maxBuffer: 64 << 20 },
    ),
  ) as { packages: Package[]; resolve: { nodes: Node[] } };
  const packages = new Map(metadata.packages.map((pkg) => [pkg.id, pkg]));
  const nodes = new Map(metadata.resolve.nodes.map((node) => [node.id, node]));
  const pending = metadata.packages
    .filter((pkg) => roots.includes(pkg.name) && pkg.source === null)
    .map((pkg) => pkg.id);
  if (pending.length !== roots.length)
    throw new Error(`Unknown workspace package in ${roots.join(", ")}`);
  const seen = new Set<string>();
  for (let id = pending.pop(); id !== undefined; id = pending.pop()) {
    if (seen.has(id)) continue;
    seen.add(id);
    for (const dep of nodes.get(id)?.deps ?? [])
      if (dep.dep_kinds.some((kind) => kind.kind !== "dev"))
        pending.push(dep.pkg);
  }
  // Workspace crates are this application, under LICENSE.
  return [...seen].flatMap((id) => {
    const pkg = packages.get(id);
    if (!pkg) throw new Error(`cargo metadata does not list ${id}`);
    return pkg.source === null ? [] : [pkg];
  });
}

/** The standard library's notices, from the toolchain that builds the binaries. */
export function rustStdNotices(): string {
  const sysroot = execFileSync("rustc", ["--print", "sysroot"], {
    encoding: "utf8",
  }).trim();
  return readFileSync(
    join(sysroot, "share/doc/rust/COPYRIGHT-library.html"),
    "utf8",
  );
}

if (import.meta.main) {
  const [target, ...roots] = process.argv.slice(2);
  if (!target || !roots.length)
    throw new Error(
      "Usage: bun scripts/rust-licenses.ts <target> <package>...",
    );
  process.stdout.write(rustLicenseText([{ target, packages: roots }]));
}
