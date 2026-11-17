import {
  body,
  mirrorballSvg,
  palette,
  writeMirrorballSpin,
} from "./mirrorball.mjs";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { SquareTerminal } from "lucide-react";
import sharp from "sharp";
import { mkdir, writeFile } from "node:fs/promises";
await mkdir("public/tray", { recursive: true });
await writeMirrorballSpin();
await writeFile(
  "public/providers/opencode.svg",
  renderToStaticMarkup(
    createElement(SquareTerminal, {
      size: 32,
      color: "#242e28",
      strokeWidth: 1.6,
    }),
  ),
);
// Menu bar / tray icon (docs/design/icons.md): a still render of the 3D
// mirrorball from Logo — tiled facets on a dark body with black seams, key
// light top-left. Orthographic projection, tilted 16° like the app logo.
const traySvg = mirrorballSvg(palette, body);
// App icon uses the same mirrorball as the renderer and tray.
await sharp({
  create: { width: 512, height: 512, channels: 4, background: "#171614" },
})
  .composite([
    {
      input: await sharp(Buffer.from(traySvg), { density: 1200 })
        .resize(310, 310)
        .png()
        .toBuffer(),
      gravity: "centre",
    },
  ])
  .png()
  .toFile("public/icon.png");

const trayStoppedSvg = mirrorballSvg(
  ["#8a8378", "#6b655c", "#9c958a", "#4a453e", "#6b655c", "#7c746a"],
  "#2a2724",
);
await writeFile("public/tray/ball.svg", traySvg);
await writeFile("public/tray/stopped.svg", trayStoppedSvg);
const renderSvg = (svg, size) =>
  sharp(Buffer.from(svg), { density: 1200 })
    .resize(size, size)
    .png()
    .toBuffer();
// macOS: 18pt at @1x/@2x. Linux trays: 22px.
for (const [name, size, retina] of [
  ["mac", 18, true],
  ["linux", 22, false],
]) {
  for (const [id, svg] of [
    ["ball", traySvg],
    ["stopped", trayStoppedSvg],
  ]) {
    await writeFile(
      `public/tray/${name}-${id}.png`,
      await renderSvg(svg, size),
    );
    if (retina)
      await writeFile(
        `public/tray/${name}-${id}@2x.png`,
        await renderSvg(svg, size * 2),
      );
  }
}

// Ship ICNS directly: electron-builder's WASM icon converter hangs under Bun.
const frames = await Promise.all(
  [
    ["icp4", 16],
    ["icp5", 32],
    ["icp6", 64],
    ["ic07", 128],
    ["ic08", 256],
    ["ic09", 512],
    ["ic11", 32],
    ["ic12", 64],
    ["ic13", 256],
    ["ic14", 512],
  ].map(async ([type, size]) => {
    const png = await sharp("public/icon.png")
      .resize(size, size)
      .png()
      .toBuffer();
    const header = Buffer.alloc(8);
    header.write(type);
    header.writeUInt32BE(png.length + 8, 4);
    return Buffer.concat([header, png]);
  }),
);
const header = Buffer.alloc(8);
header.write("icns");
header.writeUInt32BE(
  8 + frames.reduce((size, frame) => size + frame.length, 0),
  4,
);
// Only electron-builder reads the ICNS; keep it out of public/ and the renderer bundle.
await writeFile("build/icon.icns", Buffer.concat([header, ...frames]));
