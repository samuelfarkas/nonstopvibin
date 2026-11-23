// What the build ships and for which targets. Bun loads this TypeScript from the
// electron-builder hooks (CommonJS) and the build scripts (ES modules) alike; it must not
// import build tooling. The pinned core is src/server/core-binary.ts.
import { serverArchive, serverPort } from "../src/server/host-defaults.ts";

interface Architecture {
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
    uname: ["x86_64", "amd64"],
    elf: 62,
    machO: 0x01000007,
    swift: "x86_64",
  },
  arm64: {
    uname: ["aarch64", "arm64"],
    elf: 183,
    machO: 0x0100000c,
    swift: "arm64",
  },
} as const satisfies Record<string, Architecture>;
const known = new Map<string, Architecture>(Object.entries(architectures));
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
export const serverArchitectures = ["x64", "arm64"];
/** install.sh's default prefix, relative to $HOME. */
export const serverPrefix = ".local";

/**
 * The desktop app as installed builds and the update feed know it. Electron builds take the
 * same values from package.json `build`, and tests/distribution.test.mjs keeps the two equal:
 * an installed Electron build only accepts a Rust update with this identity.
 */
export const product = {
  name: "NonstopVibin",
  /** Bundle identifier, the ad-hoc designated requirement and the ShipIt cache name. */
  id: "app.nonstopvibin.desktop",
  /** The daemon on Linux, and the deb package, desktop file and icon name. */
  executable: "nonstopvibin",
  /** The WebKit window helper, beside the daemon. */
  helper: "nonstopvibin-window",
  feed: { owner: "samuelfarkas", repo: "nonstopvibin" },
  macCategory: "public.app-category.developer-tools",
  linuxCategory: "Development",
  maintainer: "nonstopvibin",
  /** SMAppService, the login item, needs macOS 13. */
  minimumMacOS: "13.0",
} as const;

/** Installers per platform, and the payload electron-updater downloads on each. */
export const desktopTargets = {
  mac: ["dmg", "zip"],
  linux: ["AppImage", "deb"],
} as const;
export const updatePayload = { mac: "zip", linux: "AppImage" } as const;
export type DesktopPlatform = keyof typeof desktopTargets;

/** electron-builder's `${productName}-${version}-${os}-${arch}.${ext}` with its arch names. */
export function desktopArtifact(
  version: string,
  platform: DesktopPlatform,
  arch: string,
  ext: string,
): string {
  architecture(arch);
  const name =
    arch !== "x64"
      ? arch
      : ext === "AppImage"
        ? "x86_64"
        : ext === "deb"
          ? "amd64"
          : arch;
  return `${product.name}-${version}-${platform}-${name}.${ext}`;
}

/**
 * Rust targets per platform and Node architecture name. The Linux daemon is static musl: a
 * glibc build links the build host's newest symbols (GLIBC_2.39 on ubuntu-24.04), which would
 * drop most distributions in docs/server.md. The helper links the system WebKitGTK, so glibc.
 */
export const rustTargets = {
  macos: { x64: "x86_64-apple-darwin", arm64: "aarch64-apple-darwin" },
  linuxDaemon: {
    x64: "x86_64-unknown-linux-musl",
    arm64: "aarch64-unknown-linux-musl",
  },
  linuxHelper: {
    x64: "x86_64-unknown-linux-gnu",
    arm64: "aarch64-unknown-linux-gnu",
  },
} as const;

/** The Rust target triple of one kind of binary for a Node architecture name. */
export function rustTarget(
  kind: keyof typeof rustTargets,
  arch: string,
): string {
  const target = new Map<string, string>(Object.entries(rustTargets[kind])).get(
    arch,
  );
  if (!target) throw new Error(`No ${kind} build for ${arch}`);
  return target;
}

/**
 * Licence files every Rust package ships in its `licenses/` folder. THIRD-PARTY.txt covers the
 * UI's npm packages (`bun run build`), THIRD-PARTY-RUST.txt the crates compiled into the
 * binaries and Rust-std.html the standard library with musl (scripts/rust-licenses.ts),
 * CLIProxyAPI.txt the core (core:install).
 */
export const rustLicenses = [
  "LICENSE",
  "NOTICE.md",
  "THIRD-PARTY.txt",
  "THIRD-PARTY-RUST.txt",
  "Rust-std.html",
  "CLIProxyAPI.txt",
] as const;

/**
 * The Rust server archive's top-level entries. The daemon embeds the UI, so there is no
 * `client/`; it finds `core/` beside its real path.
 */
export const rustServerArchive = {
  executable: "nonstopvibin",
  core: "core",
  licenses: "licenses",
  installer: "install.sh",
  readme: "README.md",
} as const;

/** scripts/server-install.sh with the values it must agree on filled in. */
export function renderInstaller(
  source: string,
  version: string,
  arch: string,
  entries: readonly string[] = Object.values(serverArchive),
): string {
  // An archive without any entry but these two cannot be installed.
  const required = entries.filter(
    (entry) =>
      entry !== serverArchive.installer && entry !== serverArchive.readme,
  );
  const values = new Map([
    ["VERSION", version],
    ["ARCH", arch],
    ["EXECUTABLE", serverArchive.executable],
    ["REQUIRED", required.join(" ")],
    ["ENTRIES", entries.join(" ")],
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
