// How the desktop shell, the CLI and the development hosts open the renderer,
// and how the renderer reads it back.

export function openedWith({ search, hash }: { search: string; hash: string }) {
  const query = new URLSearchParams(search);
  return {
    tray: query.get("view") === "tray",
    profile: query.get("profile"),
    session: new URLSearchParams(hash.slice(1)).get("session"),
  };
}
