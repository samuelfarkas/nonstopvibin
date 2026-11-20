import { build } from "esbuild";
import { mkdir, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { createRequire, isBuiltin } from "node:module";
import { dirname, join, resolve } from "node:path";
import { execFileSync } from "node:child_process";
import { tmpdir } from "node:os";
import { architecture } from "./distribution.ts";

await rm("dist/desktop", { recursive: true, force: true });
const result = await build({
  entryPoints: ["src/desktop/main.ts", "src/desktop/preload.ts"],
  bundle: true,
  minify: true,
  keepNames: true,
  platform: "node",
  format: "cjs",
  target: "node24",
  outdir: "dist/desktop",
  outExtension: { ".js": ".cjs" },
  external: ["electron"],
  metafile: true,
});
if (process.platform === "darwin")
  execFileSync(
    "xcrun",
    [
      "swiftc",
      "-O",
      "-target",
      `${architecture(process.arch).swift}-apple-macos13.0`,
      "-module-cache-path",
      join(tmpdir(), "nv-swift-module-cache"),
      "src/desktop/tray-click-monitor.swift",
      "-o",
      "dist/desktop/tray-click-monitor",
    ],
    { stdio: "inherit" },
  );
for (const output of Object.values(result.metafile.outputs))
  for (const dependency of output.imports)
    if (
      dependency.external &&
      dependency.path !== "electron" &&
      !isBuiltin(dependency.path)
    )
      throw new Error(`Unbundled desktop dependency: ${dependency.path}`);

// Keep full license texts after removing duplicated node_modules from installers.
const seen = new Set();
const notices = [];
async function licenses(manifest) {
  if (seen.has(manifest)) return;
  seen.add(manifest);
  const pkg = JSON.parse(await readFile(manifest, "utf8"));
  const directory = dirname(manifest);
  if (directory !== process.cwd()) {
    const files = (await readdir(directory))
      .filter((file) => /^(licen[cs]e|ofl)([.-]|$)/i.test(file))
      .sort();
    const text =
      pkg.name === "react-remove-scroll-bar" && pkg.version === "2.3.8"
        ? await readFile("licenses/react-remove-scroll-bar.txt", "utf8")
        : pkg.name === "lazy-val" && pkg.version === "1.0.5"
          ? await readFile("licenses/lazy-val.txt", "utf8")
          : (
              await Promise.all(
                files.map((file) => readFile(join(directory, file), "utf8")),
              )
            ).join("\n");
    if (!text) throw new Error(`Missing license text for ${pkg.name}`);
    notices.push(`${pkg.name}@${pkg.version} (${pkg.license})\n${text}`);
  }
  const require = createRequire(manifest);
  for (const name of Object.keys(pkg.dependencies ?? {}).sort())
    await licenses(require.resolve(`${name}/package.json`));
}
await licenses(resolve("package.json"));
await mkdir("dist/licenses", { recursive: true });
await writeFile(
  "dist/licenses/THIRD-PARTY.txt",
  notices.join("\n\n----------------------------------------\n\n"),
);
