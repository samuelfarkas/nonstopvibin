import type { JsonObject } from "../../src/server/json.ts";

/** One server-sent event whose data carries its own type, as Anthropic and OpenAI stream. */
export const frame = (type: string, fields: JsonObject = {}) =>
  `event: ${type}\ndata: ${JSON.stringify({ type, ...fields })}\n\n`;
/** The JSON data of every event in a stream's text, parsed as `JSON.parse` types it. */
export function events(wire: string) {
  return wire
    .split("\n")
    .filter((line) => line.startsWith("data: {"))
    .map((line) => JSON.parse(line.slice(6)));
}
