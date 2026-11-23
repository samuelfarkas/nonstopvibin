// Builds the Linux desktop app from the Rust binaries: release/NonstopVibin-<version>-linux-*.AppImage
// and .deb, and latest-linux.yml for electron-updater. Usage: bun scripts/package-linux.ts [x64|arm64]
// (default: the host). Run `bun run build` first; needs WebKitGTK 4.1 and GTK 3 development
// files for the helper, musl-tools for the static daemon, and dpkg-deb.
import {
  chmod,
  copyFile,
  cp,
  mkdir,
  mkdtemp,
  readdir,
  rm,
  stat,
  symlink,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import pkg from "../package.json" with { type: "json" };
import { coreLayout } from "./core-binary.ts";
import { desktopArtifact, product, rustTarget } from "./distribution.ts";
import {
  cargoBuild,
  releaseDir,
  repo,
  report,
  run,
  stageClient,
  stageCore,
  stageLicences,
  version,
  writeFeed,
} from "./package-common.ts";
import { releaseTool } from "./release-tools.ts";
import {
  debApp,
  desktopFile,
  iconFile,
  verifyAppDir,
  verifyArtifact,
  verifyDebTree,
} from "./verify-rust-package.ts";

const arch = process.argv[2] ?? process.arch;
const daemon = {
  target: rustTarget("linuxDaemon", arch),
  packages: ["nsv-cli"],
};
const helper = {
  target: rustTarget("linuxHelper", arch),
  packages: ["nsv-window"],
};
const binaries = { daemon: cargoBuild(daemon), helper: cargoBuild(helper) };

const desktopEntry = (exec: string) => `[Desktop Entry]
Type=Application
Name=${product.name}
Comment=${pkg.description}
Exec=${exec}
Icon=${product.executable}
Terminal=false
Categories=${product.linuxCategory};
`;
const icon = join(repo, "public/icon.png");

async function files(folder: string): Promise<string[]> {
  const entries = await readdir(folder, {
    withFileTypes: true,
    recursive: true,
  });
  return entries
    .filter((entry) => entry.isFile())
    .map((entry) => join(entry.parentPath, entry.name));
}

await mkdir(releaseDir, { recursive: true });
const work = await mkdtemp(join(tmpdir(), "nv-linux-"));
try {
  // The app folder both packages install: the daemon, its helper beside it, core/, client/
  // and licenses/.
  const app = join(work, "app");
  await mkdir(join(work, "core-stage"));
  await stageCore(
    join(work, "core-stage"),
    join(app, coreLayout.packaged),
    "linux",
    arch,
  );
  await copyFile(
    join(binaries.daemon, product.executable),
    join(app, product.executable),
  );
  await copyFile(
    join(binaries.helper, product.helper),
    join(app, product.helper),
  );
  await stageClient(join(app, "client"));
  await stageLicences(join(app, "licenses"), [daemon, helper]);
  for (const file of [
    product.executable,
    product.helper,
    join(coreLayout.packaged, coreLayout.binary),
  ])
    await chmod(join(app, file), 0o755);

  // AppImage: AppRun is the daemon, so its real path (the helper's folder) is inside the image.
  const appDir = join(work, "AppDir");
  await cp(app, appDir, { recursive: true });
  await symlink(product.executable, join(appDir, "AppRun"));
  await writeFile(join(appDir, desktopFile), desktopEntry("AppRun"));
  await copyFile(icon, join(appDir, iconFile));
  await symlink(iconFile, join(appDir, ".DirIcon"));
  verifyAppDir(appDir, arch);
  const appImage = desktopArtifact(version, "linux", arch, "AppImage");
  await rm(join(releaseDir, appImage), { force: true });
  run(
    await releaseTool("appimagetool"),
    [
      "--no-appstream",
      "--runtime-file",
      await releaseTool("appimage-runtime", arch),
      appDir,
      join(releaseDir, appImage),
    ],
    {
      env: {
        ...process.env,
        ARCH: arch === "x64" ? "x86_64" : "aarch64",
        // No FUSE on build hosts and containers.
        APPIMAGE_EXTRACT_AND_RUN: "1",
      },
    },
  );

  // deb: the app at /opt/NonstopVibin, where the Electron deb put its executable, so autostart
  // entries that run /opt/NonstopVibin/nonstopvibin keep working after an upgrade.
  const deb = join(work, "deb");
  await cp(app, join(deb, debApp), { recursive: true });
  const applications = join(deb, "usr/share/applications");
  const icons = join(deb, "usr/share/icons/hicolor/512x512/apps");
  await mkdir(applications, { recursive: true });
  await mkdir(icons, { recursive: true });
  await writeFile(
    join(applications, desktopFile),
    desktopEntry(`/${debApp}/${product.executable}`),
  );
  await copyFile(icon, join(icons, iconFile));
  verifyDebTree(deb, arch);
  let installed = 0;
  for (const file of await files(deb)) installed += (await stat(file)).size;
  const control = join(deb, "DEBIAN");
  await mkdir(control);
  const debArch = arch === "x64" ? "amd64" : arch;
  const target = `/${debApp}/${product.executable}`;
  const link = `/usr/bin/${product.executable}`;
  await writeFile(
    join(control, "control"),
    `Package: ${product.executable}
Version: ${version}
Section: devel
Priority: optional
Architecture: ${debArch}
Maintainer: ${product.maintainer}
Installed-Size: ${Math.ceil(installed / 1024)}
Depends: libwebkit2gtk-4.1-0, xdg-utils
Homepage: ${pkg.homepage}
Description: ${pkg.description}
`,
  );
  // The command link, as the Electron deb registered it.
  await writeFile(
    join(control, "postinst"),
    `#!/bin/sh
set -e
if [ "$1" = configure ]; then
  if command -v update-alternatives >/dev/null 2>&1; then
    if [ -L '${link}' ] && [ "$(readlink '${link}')" != '/etc/alternatives/${product.executable}' ]; then
      rm -f '${link}'
    fi
    update-alternatives --install '${link}' '${product.executable}' '${target}' 100
  else
    ln -sf '${target}' '${link}'
  fi
  if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q /usr/share/applications || true
  fi
fi
`,
  );
  await writeFile(
    join(control, "prerm"),
    `#!/bin/sh
set -e
if [ "$1" = remove ]; then
  if command -v update-alternatives >/dev/null 2>&1; then
    update-alternatives --remove '${product.executable}' '${target}'
  else
    rm -f '${link}'
  fi
fi
`,
  );
  await chmod(join(control, "postinst"), 0o755);
  await chmod(join(control, "prerm"), 0o755);
  const debName = desktopArtifact(version, "linux", arch, "deb");
  run("dpkg-deb", [
    "--root-owner-group",
    "-Zxz",
    "--build",
    deb,
    join(releaseDir, debName),
  ]);

  const feed = await writeFeed(releaseDir, "linux", [appImage, debName]);
  for (const name of [appImage, debName]) {
    verifyArtifact(join(releaseDir, name));
    await report(join(releaseDir, name));
  }
  console.log(`Wrote ${feed.slice(repo.length + 1)}.`);
} finally {
  await rm(work, { recursive: true, force: true });
}
