/** The lit mirrorball: tile colours and body, shared by the app icon, the tray and the logo. */
export const palette = [
  "#f0bc63",
  "#e0a33f",
  "#f5d08c",
  "#c98a2e",
  "#e0a33f",
  "#e88ab8",
  "#f0bc63",
  "#e0a33f",
];
export const body = "#1a1610";
/** One 2.4-second Y-axis turn at 30 fps; the last frame matches the first. styles.css plays it. */
export const spin = { frames: 73, size: 48, durationMs: 2400 };

export const mirrorballSvg = (palette, body, phase = 0) => {
  const R = 30;
  const tilt = (-16 * Math.PI) / 180;
  const rings = [-75, -50, -25, 0, 25, 50, 75];
  const project = (lat, lon) => {
    const x = Math.cos(lat) * Math.sin(lon);
    const y = Math.sin(lat);
    const z = Math.cos(lat) * Math.cos(lon);
    return {
      x: 32 + R * x,
      y: 32 - R * (y * Math.cos(tilt) - z * Math.sin(tilt)),
      z: y * Math.sin(tilt) + z * Math.cos(tilt),
      n: { x, y, z },
    };
  };
  const tiles = [];
  rings.forEach((latDeg, ring) => {
    const count = Math.max(
      1,
      Math.round(18 * Math.cos((latDeg * Math.PI) / 180)),
    );
    const pitch = (2 * Math.PI) / count;
    for (let i = 0; i < count; i++) {
      const lon = pitch * (i + (ring % 2) / 2) + phase;
      const lat = (latDeg * Math.PI) / 180;
      const c = project(lat, lon);
      if (c.z < 0.08) continue;
      const half = ((25 - 3) / 2) * (Math.PI / 180);
      const w = count === 1 ? Math.PI / 2 : (pitch - 0.045) / 2;
      const corners = [
        project(lat - half, lon - w),
        project(lat - half, lon + w),
        project(lat + half, lon + w),
        project(lat + half, lon - w),
      ];
      const light = { x: -0.45, y: 0.55, z: 0.7 };
      const lum = Math.max(
        0,
        c.n.x * light.x + c.n.y * light.y + c.n.z * light.z,
      );
      const fill = palette[(i * 5 + ring * 3) % palette.length];
      tiles.push(
        `<polygon points="${corners.map((p) => `${p.x.toFixed(2)},${p.y.toFixed(2)}`).join(" ")}" fill="${fill}" opacity="${(0.45 + 0.55 * lum).toFixed(2)}"/>`,
      );
    }
  });
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">
<defs>
<radialGradient id="k" cx="0.32" cy="0.26" r="0.5"><stop offset="0" stop-color="#fff" stop-opacity=".75"/><stop offset=".35" stop-color="#fff" stop-opacity=".18"/><stop offset="1" stop-color="#fff" stop-opacity="0"/></radialGradient>
<radialGradient id="v" cx="0.5" cy="0.5" r="0.5"><stop offset=".6" stop-color="#000" stop-opacity="0"/><stop offset="1" stop-color="#000" stop-opacity=".7"/></radialGradient>
<clipPath id="c"><circle cx="32" cy="32" r="31"/></clipPath>
</defs>
<circle cx="32" cy="32" r="31" fill="${body}"/>
<g clip-path="url(#c)">${tiles.join("")}</g>
<circle cx="32" cy="32" r="31" fill="url(#k)"/>
<circle cx="32" cy="32" r="31" fill="url(#v)"/>
</svg>`;
};

export async function writeMirrorballSpin() {
  const { default: sharp } = await import("sharp");
  const frames = await Promise.all(
    Array.from({ length: spin.frames }, async (_, frame) => ({
      input: await sharp(
        Buffer.from(
          mirrorballSvg(
            palette,
            body,
            (frame * Math.PI * 2) / (spin.frames - 1),
          ),
        ),
      )
        .resize(spin.size, spin.size)
        .png()
        .toBuffer(),
      left: frame * spin.size,
      top: 0,
    })),
  );
  await sharp({
    create: {
      width: spin.frames * spin.size,
      height: spin.size,
      channels: 4,
      background: "#0000",
    },
  })
    .composite(frames)
    .png()
    .toFile("public/mirrorball-spin.png");
}
