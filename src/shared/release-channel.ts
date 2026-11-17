// Read by scripts/verify-release.mjs (which the release workflow asks) and the desktop updater.
const releaseVersion =
  /^(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)(-beta\.(?:0|[1-9]\d*))?$/;

/** A version this project releases is X.Y.Z, or X.Y.Z-beta.N for a prerelease. */
export function releaseChannel(
  version: string,
): "release" | "prerelease" | undefined {
  const match = releaseVersion.exec(version);
  if (!match) return undefined;
  return match[1] ? "prerelease" : "release";
}
