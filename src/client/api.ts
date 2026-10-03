import type { Json } from "../shared/types.ts";

let token = "";
export async function initializeSession(): Promise<void> {
  if (window.nonstopvibin)
    token = (await window.nonstopvibin.bootstrap()).token;
  else {
    const params = new URLSearchParams(location.hash.slice(1));
    token = params.get("session") || sessionStorage.getItem("nv-session") || "";
    if (token) sessionStorage.setItem("nv-session", token);
    if (params.has("session"))
      history.replaceState(null, "", location.pathname + location.search);
    // Pasting a new session link into an open tab only changes the fragment;
    // reload so it replaces the stale token.
    addEventListener("hashchange", () => {
      if (new URLSearchParams(location.hash.slice(1)).has("session"))
        location.reload();
    });
  }
}
export async function api<T>(
  path: string,
  method = "GET",
  body?: Json,
  signal?: AbortSignal,
): Promise<T> {
  const headers = new Headers({ Authorization: `Bearer ${token}` });
  if (body !== undefined) headers.set("Content-Type", "application/json");
  const response = await fetch(`/api${path}`, {
    method,
    signal,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const data = await response.json();
  if (!response.ok)
    throw Object.assign(
      new Error(data.error?.message || `Request failed (${response.status}).`),
      { status: response.status },
    );
  // SAFETY: the local server owns both ends of this contract; each caller names
  // the response type its route returns.
  return data as T;
}
export async function copy(text: string): Promise<void> {
  if (window.nonstopvibin) await window.nonstopvibin.copy(text);
  else await navigator.clipboard.writeText(text);
}
export async function openExternal(url: string): Promise<void> {
  if (window.nonstopvibin) await window.nonstopvibin.openExternal(url);
  else window.open(url, "_blank", "noopener,noreferrer");
}
