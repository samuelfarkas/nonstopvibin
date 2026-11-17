import { z } from "zod";
import type { Json } from "../shared/types.ts";

export type JsonObject = { [key: string]: Json | undefined };

/** Schema for values already decoded by JSON.parse or response.json(). */
const json = z.json();

/** Decodes JSON text. Throws like JSON.parse; callers own the failure message. */
export function parse(content: string): Json {
  return json.parse(JSON.parse(content));
}
/** Decodes a fetch response body. Throws like response.json(). */
export async function responseJson(response: Response): Promise<Json> {
  return json.parse(await response.json());
}
/** Returns the value when it is a JSON object, otherwise an empty object. */
export function record(value: Json | undefined): JsonObject {
  return value instanceof Object && !Array.isArray(value) ? value : {};
}
/** Returns the value when it is a string, otherwise undefined. */
export function text(value: Json | undefined): string | undefined {
  return z.string().safeParse(value).data;
}
/** Providers send counters as numbers or numeric strings; anything else is unknown. */
export function number(value: Json | undefined): number | null {
  const n = z.union([z.number(), z.string().min(1)]).safeParse(value).data;
  if (n === undefined) return null;
  const parsed = Number(n);
  return Number.isFinite(parsed) ? parsed : null;
}
/** Some management endpoints wrap a JSON document inside a JSON string. */
export function unwrap(value: Json | undefined): Json | undefined {
  const wrapped = text(value);
  return wrapped === undefined ? value : parse(wrapped);
}
