import type {
  RouteBody,
  RouteKey,
  RouteParamName,
  RouteParams,
  RouteQuery,
  RouteResponse,
  SessionRefused,
} from "../shared/api.ts";
import { openedWith } from "../shared/renderer-url.ts";

/** The desktop shell's bridge; absent in a browser and in server-side test renders. */
export const desktop = globalThis.window?.nonstopvibin;
let token = "";
export async function initializeSession(): Promise<void> {
  if (desktop) token = (await desktop.bootstrap()).token;
  else {
    const { session } = openedWith(location);
    token = session || sessionStorage.getItem("nv-session") || "";
    if (token) sessionStorage.setItem("nv-session", token);
    if (session !== null)
      history.replaceState(null, "", location.pathname + location.search);
    // Pasting a new session link into an open tab only changes the fragment;
    // reload so it replaces the stale token.
    addEventListener("hashchange", () => {
      if (openedWith(location).session !== null) location.reload();
    });
  }
}
type ParamSlot<K extends RouteKey> = [RouteParamName<K>] extends [never]
  ? { params?: never }
  : { params: RouteParams<K> };
type QuerySlot<Query> = [Query] extends [undefined]
  ? { query?: never }
  : Partial<Query> extends Query
    ? { query?: Query }
    : { query: Query };
type BodySlot<Body> = [Body] extends [undefined]
  ? { body?: never }
  : { body: Body };
export type ApiCall<K extends RouteKey> = ParamSlot<K> &
  QuerySlot<RouteQuery<K>> &
  BodySlot<RouteBody<K>> & { signal?: AbortSignal };
interface SentCall<K extends RouteKey> {
  params?: RouteParams<K>;
  query?: RouteQuery<K>;
  body?: RouteBody<K>;
  signal?: AbortSignal;
}
export function api<K extends RouteKey>(
  route: K,
  ...call: Partial<ApiCall<K>> extends ApiCall<K>
    ? [call?: ApiCall<K>]
    : [call: ApiCall<K>]
): Promise<RouteResponse<K>>;
export async function api<K extends RouteKey>(
  route: K,
  { params, query, body, signal }: SentCall<K> = {},
): Promise<RouteResponse<K>> {
  const [method, template] = route.split(" ");
  const values = new Map(Object.entries<string>(params ?? {}));
  const path = template
    .split("/")
    .map((part) =>
      part.startsWith(":")
        ? encodeURIComponent(values.get(part.slice(1)) ?? "")
        : part,
    )
    .join("/");
  const search = new URLSearchParams();
  for (const [name, value] of Object.entries(query ?? {}))
    if (value !== undefined) search.set(name, String(value));
  const headers = new Headers({ Authorization: `Bearer ${token}` });
  if (body !== undefined) headers.set("Content-Type", "application/json");
  const response = await fetch(`/api${path}${String(search) && `?${search}`}`, {
    method,
    signal,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  let data;
  try {
    data = await response.json();
  } catch (error) {
    if (response.ok) throw error;
    data = {};
  }
  if (!response.ok)
    throw Object.assign(
      new Error(data.error?.message || `Request failed (${response.status}).`),
      { status: response.status },
    );
  // Unchecked: the route's response type is the server's wire type for it.
  return data;
}
const sessionRefused: SessionRefused = 401;
/** The server refused the session token; its message says how to open a new session. */
export function refusedSession(error: Error): boolean {
  return "status" in error && error.status === sessionRefused;
}
export async function copy(text: string): Promise<void> {
  if (desktop) await desktop.copy(text);
  else await navigator.clipboard.writeText(text);
}
export async function openExternal(url: string): Promise<void> {
  if (desktop) await desktop.openExternal(url);
  else window.open(url, "_blank", "noopener,noreferrer");
}
