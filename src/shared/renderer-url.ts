// How the desktop shell, the CLI and the development hosts open the renderer,
// and how the renderer reads it back.

export function mainUrl(origin: string, profileId?: string): string {
  return profileId
    ? `${origin}/?profile=${encodeURIComponent(profileId)}`
    : origin;
}
export const trayUrl = (origin: string) => `${origin}/?view=tray`;
/** A browser link that carries the session token; the fragment never reaches the server. */
export const sessionUrl = (origin: string, token: string) =>
  `${origin}/#session=${token}`;

export function openedWith({ search, hash }: { search: string; hash: string }) {
  const query = new URLSearchParams(search);
  return {
    tray: query.get("view") === "tray",
    profile: query.get("profile"),
    session: new URLSearchParams(hash.slice(1)).get("session"),
  };
}
