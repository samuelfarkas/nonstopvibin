// Shared pieces of the golden recorder: the case shape, placeholder normalisation,
// error capture and the writer. See README.md for the file format.
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { z } from "zod";
import { AppError } from "../../src/server/errors.ts";
import type { Json } from "../../src/shared/types.ts";

export const goldenDirectory = dirname(fileURLToPath(import.meta.url));
export const repository = join(goldenDirectory, "../..");

/** What a case holds: any JSON-serialisable value. */
export type Recorded = Json | object | undefined;
/** `thrown` names a non-AppError the server would answer with a generic 500. */
export type CaseError = { message?: string; status: number; thrown?: string };
export type Case =
  | { name: string; input: Recorded; output: Recorded }
  | { name: string; input: Recorded; error: CaseError };

/** AppError message and status; a zod error's status only (400, as the server answers); another Error's name. */
export function caseError(cause: unknown): CaseError {
  if (cause instanceof AppError)
    return { message: cause.message, status: cause.status };
  if (cause instanceof z.ZodError) return { status: 400 };
  if (cause instanceof Error)
    return { message: cause.message, status: 500, thrown: cause.name };
  throw cause;
}

/** A dependency stub with only the members the code under test calls. */
export function stub<T>(members: Partial<T>): T {
  // SAFETY: each caller names the members its code path reads; any other access fails loudly.
  return members as T;
}

/** Runs `run` and records its result or its AppError/zod error. */
export async function outcome<Input extends Recorded>(
  name: string,
  input: Input,
  run: () => Recorded | Promise<Recorded>,
): Promise<Case> {
  try {
    return { name, input, output: await run() };
  } catch (error) {
    return { name, input, error: caseError(error) };
  }
}

/**
 * Replaces generated values (temp paths, UUIDs, ports, tokens) with placeholders in
 * every string of a JSON value. Longer values are replaced first.
 */
export class Placeholders {
  private readonly pairs = new Map<string, string>();
  add(value: string | number | undefined, placeholder: string): this {
    if (value !== undefined && String(value) !== "")
      this.pairs.set(String(value), placeholder);
    return this;
  }
  text(value: string): string {
    let result = value;
    for (const [from, to] of [...this.pairs].sort(
      (a, b) => b[0].length - a[0].length,
    ))
      result = result.replaceAll(from, to);
    return result;
  }
  /** On the serialised JSON, so object keys (some are generated IDs) change too. */
  apply<T extends Recorded>(value: T): T {
    let serialised = JSON.stringify(value);
    for (const [from, to] of [...this.pairs].sort(
      (a, b) => b[0].length - a[0].length,
    ))
      serialised = serialised.replaceAll(
        JSON.stringify(from).slice(1, -1),
        JSON.stringify(to).slice(1, -1),
      );
    return JSON.parse(serialised);
  }
}

/** A fresh scratch directory at a fixed path, so runs see the same paths. */
export function scratch(name: string): string {
  const directory = join("/tmp/nsv-golden", name);
  rmSync(directory, { recursive: true, force: true });
  mkdirSync(directory, { recursive: true, mode: 0o700 });
  return directory;
}

/** One case per line keeps the files compact and their diffs readable. */
export function writeArea(area: string, cases: Case[]): number {
  const names = new Set<string>();
  for (const c of cases) {
    if (names.has(c.name)) throw new Error(`${area}: duplicate case ${c.name}`);
    names.add(c.name);
  }
  writeFileSync(
    join(goldenDirectory, `${area}.json`),
    `[\n${cases.map((c) => JSON.stringify(c)).join(",\n")}\n]\n`,
  );
  return cases.length;
}

export const syntheticKey = (letter: string) => `nv_${letter.repeat(43)}`;

/** A fixed models.dev payload: valid, tiered, cache-less, image-only and malformed models. */
export const modelsDev = {
  anthropic: {
    models: {
      "claude-sonnet-4-5": {
        name: "Claude Sonnet 4.5",
        limit: { context: 200_000, output: 64_000 },
        modalities: { input: ["text", "image", "pdf"], output: ["text"] },
        reasoning: true,
        tool_call: true,
        cost: {
          input: 3,
          output: 15,
          cache_read: 0.3,
          cache_write: 3.75,
          tiers: [
            {
              input: 6,
              output: 22.5,
              cache_read: 0.6,
              cache_write: 7.5,
              tier: { type: "context", size: 200_000 },
            },
          ],
        },
      },
      "claude-opus-4-1": {
        name: "Claude Opus 4.1",
        limit: { context: 200_000, output: 32_000, input: 180_000 },
        modalities: { input: ["text"], output: ["text"] },
        reasoning: true,
        tool_call: true,
        cost: { input: 15, output: 75, cache_read: 1.5, cache_write: 18.75 },
      },
      "claude-haiku-legacy": {
        name: "Claude Haiku (no limits)",
        modalities: { input: ["text"], output: ["text"] },
        reasoning: false,
        tool_call: true,
      },
    },
  },
  openai: {
    models: {
      "gpt-5": {
        name: "GPT-5",
        limit: { context: 400_000, output: 128_000 },
        modalities: { input: ["text", "image"], output: ["text"] },
        reasoning: true,
        tool_call: true,
        cost: { input: 1.25, output: 10, cache_read: 0.125, reasoning: 12 },
      },
      "gpt-5-codex": {
        name: "GPT-5-Codex",
        limit: { context: 400_000, output: 128_000 },
        modalities: { input: ["text"], output: ["text"] },
        reasoning: true,
        tool_call: true,
        cost: { input: 1.25, output: 10 },
      },
      "gpt-image-1": {
        name: "GPT Image 1",
        limit: { context: 32_000, output: 4_096 },
        modalities: { input: ["text", "image"], output: ["image"] },
        reasoning: false,
        tool_call: false,
        cost: { input: 5, output: 40 },
      },
    },
  },
  google: {
    models: {
      "gemini-2.5-pro": {
        name: "Gemini 2.5 Pro",
        limit: { context: 1_048_576, output: 65_536 },
        modalities: { input: ["text", "image", "audio"], output: ["text"] },
        reasoning: true,
        tool_call: true,
        cost: { input: 1.25, output: 10, cache_read: 0.31 },
      },
      "gemini-free": {
        name: "Gemini Free",
        limit: { context: 1_000, output: 1_000 },
        modalities: { input: ["text"], output: ["text"] },
        reasoning: false,
        tool_call: true,
      },
    },
  },
  "kimi-for-coding": {
    models: {
      "kimi-k2": {
        name: "Kimi K2",
        limit: { context: 262_144, output: 32_768 },
        modalities: { input: ["text"], output: ["text"] },
        reasoning: false,
        tool_call: true,
        cost: { input: 0, output: 0, cache_read: 0, cache_write: 0 },
      },
    },
  },
  "opencode-go": {
    models: {
      "glm-4.6": {
        name: "GLM 4.6",
        limit: { context: 204_800, output: 131_072 },
        modalities: { input: ["text"], output: ["text"] },
        reasoning: true,
        tool_call: true,
        cost: { input: 0.6, output: 2.2, cache_read: 0.11, cache_write: 0 },
      },
      "bad-price": {
        name: "Negative",
        limit: { context: 1, output: 1 },
        modalities: { input: ["text"], output: ["text"] },
        reasoning: false,
        tool_call: true,
        cost: { input: -1, output: 1 },
      },
    },
  },
};

/** A fetch that answers each request with `answer(url, init)`. */
export function fetchStub(
  answer: (url: string, init?: RequestInit) => Response | Promise<Response>,
): typeof fetch {
  const stubbed = async (input: string | URL | Request, init?: RequestInit) =>
    answer(String(input), init);
  // SAFETY: the code under test only calls fetch; Bun's extra members (preconnect) are never read.
  return stubbed as typeof fetch;
}

/** A fetch that answers models.dev with `modelsDev` and fails everything else. */
export const modelsDevFetch = fetchStub((url) => {
  if (url === "https://models.dev/api.json")
    return new Response(JSON.stringify(modelsDev));
  throw new Error(`Unexpected fetch ${url}`);
});
