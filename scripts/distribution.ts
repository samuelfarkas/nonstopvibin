// What the build ships and for which targets. Bun loads this TypeScript from the
// electron-builder hooks (CommonJS) and the build scripts (ES modules) alike; it must not
// import build tooling. The pinned core is src/server/core-binary.ts.
import { serverArchive, serverPort } from "../src/server/host-defaults.ts";

interface Architecture {
  /** The core release's archive suffix. */
  core: string;
  /** What `uname -m` prints on a Linux server. */
  uname: readonly string[];
  /** ELF `e_machine`. */
  elf: number;
  /** Mach-O `cputype`. */
  machO: number;
  /** swiftc's target triple prefix. */
  swift: string;
}
/** Each supported architecture by Node's name, with the names other tools give it. */
export const architectures = {
  x64: {
    core: "amd64",
    uname: ["x86_64", "amd64"],
    elf: 62,
    machO: 0x01000007,
    swift: "x86_64",
  },
  arm64: {
    core: "aarch64",
    uname: ["aarch64", "arm64"],
    elf: 183,
    machO: 0x0100000c,
    swift: "arm64",
  },
} as const satisfies Record<string, Architecture>;
const known = new Map<string, Architecture>(Object.entries(architectures));
/** The names of a Node architecture; throws for one this project does not ship. */
export function architecture(arch: string): Architecture {
  const names = known.get(arch);
  if (!names) throw new Error(`Unsupported architecture: ${arch}`);
  return names;
}

/** Files that must never ship in a desktop package or a server archive. */
export const privateFilePattern =
  /(?:^|\/)(?:\.env(?:\.[^/]*)?|\.runtime|\.test-runtime|\.deepsec|\.agents|\.git|\.codex|\.claude|\.playwright-cli|id_rsa|id_ed25519|auth\.json|credentials\.json|tokens\.json)(?:\/|$)|\.(?:pem|key|p12|pfx|sqlite3?(?:-(?:wal|shm))?|db(?:-(?:wal|shm))?|map)$/;

/** The headless server archive's directory name; the archive adds `.tar.gz`. */
export const serverArchiveName = (version: string, arch: string): string =>
  `nonstopvibin-server-${version}-linux-${arch}`;
/** The headless server's architectures, in build order. */
export const serverArchitectures = ["x64", "arm64"];
/** install.sh's default prefix, relative to $HOME. */
export const serverPrefix = ".local";

/** scripts/server-install.sh with the values it must agree on filled in. */
export function renderInstaller(
  source: string,
  version: string,
  arch: string,
): string {
  const required = [
    serverArchive.executable,
    serverArchive.client,
    serverArchive.core,
    serverArchive.licenses,
  ];
  const values = new Map([
    ["VERSION", version],
    ["ARCH", arch],
    ["EXECUTABLE", serverArchive.executable],
    ["REQUIRED", required.join(" ")],
    [
      "ENTRIES",
      [...required, serverArchive.installer, serverArchive.readme].join(" "),
    ],
    [
      "MACHINES",
      Object.entries(architectures)
        .flatMap(([name, { uname }]) => uname.map((m) => `${name}:${m}`))
        .join(" "),
    ],
    ["DEFAULT_PORT", String(serverPort)],
    ["DEFAULT_PREFIX", serverPrefix],
  ]);
  const unused = new Set(values.keys());
  const script = source.replace(/@([A-Z_]+)@/g, (_, key: string) => {
    const value = values.get(key);
    if (value === undefined) throw new Error(`install.sh: unknown @${key}@`);
    // Every value lands inside double quotes in the script.
    if (!/^[\w.: -]+$/.test(value))
      throw new Error(`install.sh: unsafe value for ${key}: ${value}`);
    unused.delete(key);
    return value;
  });
  if (unused.size)
    throw new Error(`install.sh does not use ${[...unused].join(", ")}`);
  return script;
}
