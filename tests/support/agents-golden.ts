// The agent sources the daemon writes, as tests/golden/agents.json records them byte for byte,
// for the harnesses that run them (tests/generated-agent-scripts.test.ts and
// scripts/check-pi-profiles.mjs).
import assert from "node:assert/strict";
import { z } from "zod";
import cases from "../golden/agents.json" with { type: "json" };

/** A scenario's steps as the golden records them: the files each step changed. */
const stepsSchema = z.array(
  z.object({
    changes: z
      .record(
        z.string(),
        z.object({ content: z.string().optional() }).nullable(),
      )
      .optional(),
  }),
);

function output(name: string) {
  const found = cases.find((entry) => entry.name === name);
  assert.ok(found, `golden case ${name}`);
  return found.output;
}

/** The text the golden case `name` records, such as a generated source. */
export const goldenText = (name: string): string =>
  z.string().parse(output(name));

/** The content a scenario's first step wrote to `path`, placeholders and all. */
export function firstWrite(scenario: string, path: string): string {
  const [first] = stepsSchema.parse(output(scenario));
  const content = first?.changes?.[path]?.content;
  assert.ok(content, `${scenario} writes ${path}`);
  return content;
}

/** `text` with each placeholder replaced by its value, in order. */
export const fill = (
  text: string,
  places: Iterable<readonly [string, string]>,
): string =>
  [...places].reduce(
    (result, [placeholder, value]) => result.replaceAll(placeholder, value),
    text,
  );
