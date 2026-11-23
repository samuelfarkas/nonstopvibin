// `bun run build:client`: writes `uiLicences`, the full license texts of the npm packages the
// interface bundles (package.json `dependencies` and theirs), which every package ships in
// licenses/.
import { mkdir, readFile, readdir, writeFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { dirname, join, resolve } from "node:path";
import { uiLicences } from "./distribution.ts";

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
await mkdir(dirname(uiLicences), { recursive: true });
await writeFile(
  uiLicences,
  notices.join("\n\n----------------------------------------\n\n"),
);
