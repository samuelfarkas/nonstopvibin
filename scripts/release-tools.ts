// `bun scripts/release-tools.ts <tool>...`: downloads the packaging tools the Rust release uses
// into .vendor/release-tools, each checked against the SHA-256 reviewed here, and prints their
// paths. Tools: appimagetool, appimage-runtime (the AppImage type 2 runtime), minisign.
// Like scripts/install-security-tools.mjs, a new version means a new reviewed hash.
import { chmod, mkdir, readFile, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { type Pin, pinnedDownload, sha256 } from "./pinned-download.ts";

/** A plain download is the tool; from a .tar.gz the tool is the member. */
type Download = Pin & { member?: { path: string; sha256: string } };
type Machine = "x86_64" | "aarch64";

// appimagetool 1.9.1 and type2-runtime 20251108: GitHub's asset digests. minisign 0.12: its
// .minisig verified with the author's key RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3.
const tools = new Map<string, Partial<Record<Machine, Download>>>(
  Object.entries({
    appimagetool: {
      x86_64: {
        url: "https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-x86_64.AppImage",
        sha256:
          "ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0",
      },
      aarch64: {
        url: "https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-aarch64.AppImage",
        sha256:
          "f0837e7448a0c1e4e650a93bb3e85802546e60654ef287576f46c71c126a9158",
      },
    },
    "appimage-runtime": {
      x86_64: {
        url: "https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-x86_64",
        sha256:
          "2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d",
      },
      aarch64: {
        url: "https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-aarch64",
        sha256:
          "00cbdfcf917cc6c0ff6d3347d59e0ca1f7f45a6df1a428a0d6d8a78664d87444",
      },
    },
    minisign: {
      x86_64: {
        url: "https://github.com/jedisct1/minisign/releases/download/0.12/minisign-0.12-linux.tar.gz",
        sha256:
          "9a599b48ba6eb7b1e80f12f36b94ceca7c00b7a5173c95c3efc88d9822957e73",
        member: {
          path: "minisign-linux/x86_64/minisign",
          sha256:
            "2c74dffcc1c9a5ee55957c60971998ace2b89f22585631594ec2152c588af8db",
        },
      },
    },
  }),
);

const machines = new Map<string, Machine>([
  ["x64", "x86_64"],
  ["arm64", "aarch64"],
]);

/** The tool's path in .vendor/release-tools, downloading and checking it first if needed. */
export async function releaseTool(
  name: string,
  arch: string = process.arch,
): Promise<string> {
  const machine = machines.get(arch);
  const download = machine && tools.get(name)?.[machine];
  if (!download || process.platform !== "linux")
    throw new Error(`No pinned ${name} for ${process.platform}/${arch}`);
  const destination = resolve(
    import.meta.dirname,
    "../.vendor/release-tools",
    `${name}-${machine}`,
  );
  // A cached copy is trusted only while it is the reviewed file.
  const cached = await readFile(destination).catch(() => undefined);
  const expected = download.member?.sha256 ?? download.sha256;
  if (cached && sha256(cached) === expected) return destination;
  const tool = await pinnedDownload(name, download, 120_000);
  await mkdir(join(destination, ".."), { recursive: true });
  await writeFile(destination, tool);
  await chmod(destination, 0o755);
  return destination;
}

if (import.meta.main)
  for (const name of process.argv.slice(2))
    console.log(await releaseTool(name));
