// The core's config.yaml, as text and as the object ConfigWriter returns, for profiles
// with varied API accounts; applied() comparisons; profile slugs from createProfile.
import { mkdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import YAML from "yaml";
import { apiAccount } from "../../../src/server/api-accounts.ts";
import {
  applied,
  ConfigWriter,
  type CoreConfiguration,
} from "../../../src/server/config.ts";
import { coreConfigPath } from "../../../src/server/data-directory.ts";
import { secretIds, Store } from "../../../src/server/store.ts";
import { fileKeyCodec } from "../../../src/server/vault.ts";
import type { ApiAccountInput } from "../../../src/shared/api.ts";
import type { Json, Profile } from "../../../src/shared/types.ts";
import { type Case, outcome, Placeholders, scratch } from "../lib.ts";

type AccountSpec = ApiAccountInput & { disabled?: boolean };
interface ConfigInput {
  profile: Pick<Profile, "strategy" | "sessionAffinity" | "enabled">;
  port: number;
  accounts: AccountSpec[];
}

const custom = (
  name: string,
  fields: Partial<ApiAccountInput> & Pick<ApiAccountInput, "models">,
): AccountSpec => ({
  name,
  provider: "custom",
  baseUrl: "http://127.0.0.1:11434/v1",
  apiKey: `synthetic key ${name}`,
  protocol: "openai",
  prefix: "",
  ...fields,
});
const go = (
  name: string,
  models: string[],
  extra: Partial<AccountSpec> = {},
): AccountSpec => ({
  name,
  provider: "opencode-go",
  baseUrl: "https://opencode.ai/zen/go/v1",
  apiKey: `synthetic key ${name}`,
  protocol: "openai",
  prefix: "go",
  models: models.map((id) => ({ id, protocol: "openai" })),
  ...extra,
});
const fillFirst = {
  strategy: "fill-first",
  sessionAffinity: true,
  enabled: true,
} as const;
const roundRobin = {
  strategy: "round-robin",
  sessionAffinity: false,
  enabled: false,
} as const;

const configCases: Array<[string, ConfigInput]> = [
  [
    "no accounts fill-first affinity",
    { profile: fillFirst, port: 8317, accounts: [] },
  ],
  [
    "no accounts round-robin no affinity",
    { profile: roundRobin, port: 65535, accounts: [] },
  ],
  [
    "custom openai with prefix",
    {
      profile: fillFirst,
      port: 8318,
      accounts: [
        custom("local", {
          prefix: "local",
          models: [{ id: "qwen3-coder:30b", protocol: "openai" }],
        }),
      ],
    },
  ],
  [
    "custom without prefix trailing slash",
    {
      profile: fillFirst,
      port: 8318,
      accounts: [
        custom("plain", {
          baseUrl: "http://localhost:8080/v1/",
          models: [{ id: "llama", protocol: "openai" }],
        }),
      ],
    },
  ],
  [
    "custom with every protocol shares one headers object",
    {
      profile: roundRobin,
      port: 8319,
      accounts: [
        custom("mixed", {
          prefix: "mix",
          models: [
            { id: "chat-model", protocol: "openai" },
            { id: "claude-proxy", protocol: "anthropic" },
            { id: "responses-model", protocol: "responses" },
            { id: "second-chat", protocol: "openai" },
          ],
        }),
      ],
    },
  ],
  [
    "anthropic base urls drop v1",
    {
      profile: fillFirst,
      port: 8320,
      accounts: [
        {
          name: "Anthropic",
          provider: "anthropic",
          baseUrl: "https://api.anthropic.com/v1",
          apiKey: "synthetic key anthropic",
          protocol: "anthropic",
          prefix: "anthropic",
          models: [{ id: "claude-sonnet-4-5", protocol: "anthropic" }],
        },
        custom("proxy-v1-slash", {
          baseUrl: "https://proxy.example.com/v1/",
          models: [{ id: "c1", protocol: "anthropic" }],
        }),
        custom("proxy-no-v1", {
          baseUrl: "https://proxy.example.com/api",
          models: [{ id: "c2", protocol: "anthropic" }],
        }),
        custom("proxy-v1-middle", {
          baseUrl: "https://proxy.example.com/v1/anthropic",
          models: [{ id: "c3", protocol: "anthropic" }],
        }),
        custom("responses-keeps-v1", {
          baseUrl: "https://proxy.example.com/v1",
          models: [{ id: "r1", protocol: "responses" }],
        }),
      ],
    },
  ],
  [
    "openai preset chat and responses",
    {
      profile: fillFirst,
      port: 8321,
      accounts: [
        {
          name: "OpenAI",
          provider: "openai",
          baseUrl: "https://api.openai.com/v1",
          apiKey: "synthetic key openai",
          protocol: "openai",
          prefix: "oai",
          models: [
            { id: "gpt-5", protocol: "openai" },
            { id: "gpt-5-codex", protocol: "responses" },
          ],
        },
      ],
    },
  ],
  [
    "gemini openai endpoint",
    {
      profile: fillFirst,
      port: 8322,
      accounts: [
        {
          name: "Gemini",
          provider: "gemini",
          baseUrl: "https://generativelanguage.googleapis.com/v1beta/openai",
          apiKey: "synthetic key gemini",
          protocol: "openai",
          prefix: "gem",
          models: [{ id: "gemini-2.5-pro", protocol: "openai" }],
        },
      ],
    },
  ],
  [
    "opencode go picks protocols and adds headers",
    {
      profile: fillFirst,
      port: 8323,
      accounts: [
        go("go", [
          "glm-4.6",
          "kimi-k2",
          "gpt-5-mini",
          "grok-4",
          "muse-1",
          "minimax-m2",
          "qwen3-coder",
          "qwen-max",
        ]),
      ],
    },
  ],
  [
    "two opencode go accounts with the same models merge keys",
    {
      profile: roundRobin,
      port: 8324,
      accounts: [
        go("go-one", ["glm-4.6", "qwen3-coder"]),
        go("go-two", ["glm-4.6", "qwen3-coder"]),
      ],
    },
  ],
  [
    "two opencode go accounts with different models stay apart",
    {
      profile: roundRobin,
      port: 8324,
      accounts: [
        go("go-one", ["glm-4.6"]),
        go("go-two", ["glm-4.6", "kimi-k2"]),
      ],
    },
  ],
  [
    "same compatible endpoint under two providers",
    {
      profile: fillFirst,
      port: 8325,
      accounts: [
        custom("a", {
          baseUrl: "https://api.openai.com/v1",
          models: [{ id: "gpt-5", protocol: "openai" }],
        }),
        {
          name: "b",
          provider: "openai",
          baseUrl: "https://api.openai.com/v1",
          apiKey: "synthetic key b",
          protocol: "openai",
          prefix: "",
          models: [{ id: "gpt-5", protocol: "openai" }],
        },
        custom("c", {
          baseUrl: "https://api.openai.com/v1",
          models: [{ id: "gpt-5", protocol: "openai" }],
        }),
      ],
    },
  ],
  [
    "disabled accounts are left out",
    {
      profile: fillFirst,
      port: 8326,
      accounts: [
        custom("on", { models: [{ id: "on-model", protocol: "openai" }] }),
        {
          ...custom("off", {
            models: [{ id: "off-model", protocol: "anthropic" }],
          }),
          disabled: true,
        },
        { ...go("go-off", ["glm-4.6"]), disabled: true },
      ],
    },
  ],
  [
    "model ids yaml must quote",
    {
      profile: fillFirst,
      port: 8327,
      accounts: [
        custom("quoting", {
          prefix: "q",
          models: [
            "on",
            "null",
            "yes",
            "1e3",
            "0123",
            "@scope/model",
            "a: b",
            "- dash",
            "x #y",
            "modèle-é",
            "tab\there",
            "'single'",
            '"double"',
            "trailing ",
            "*star",
            "&amp",
            "!tag",
            "%percent",
            "{brace}",
            "[bracket]",
            "",
          ].map((id) => ({ id, protocol: "openai" as const })),
        }),
      ],
    },
  ],
  [
    "prefix and key with specials",
    {
      profile: fillFirst,
      port: 8328,
      accounts: [
        custom("specials", {
          prefix: "my prefix: x",
          apiKey: "key: with # specials",
          models: [{ id: "m", protocol: "anthropic" }],
        }),
      ],
    },
  ],
];

const appliedCases: Array<[string, Json]> = [
  ["exact", "<expected>"],
  [
    "zero values omitted",
    {
      routing: {
        strategy: "fill-first",
        "session-affinity": true,
        "session-affinity-ttl": "1h",
      },
    },
  ],
  [
    "missing routing",
    { "openai-compatibility": [], "claude-api-key": [], "codex-api-key": [] },
  ],
  [
    "null lists",
    {
      routing: {
        strategy: "fill-first",
        "session-affinity": true,
        "session-affinity-ttl": "1h",
      },
      "openai-compatibility": null,
      "claude-api-key": null,
      "codex-api-key": null,
    },
  ],
  [
    "other strategy",
    {
      routing: {
        strategy: "round-robin",
        "session-affinity": true,
        "session-affinity-ttl": "1h",
      },
    },
  ],
  [
    "extra fields ignored",
    {
      routing: {
        strategy: "fill-first",
        "session-affinity": true,
        "session-affinity-ttl": "1h",
        extra: 1,
      },
      port: 1,
    },
  ],
  ["not an object", "text"],
  ["array", []],
];

const parsedYaml = (text: string): Json => YAML.parse(text);
const copy = (value: CoreConfiguration): Json =>
  JSON.parse(JSON.stringify(value));

export async function recordConfig(): Promise<Case[]> {
  const directory = scratch("config");
  const store = new Store(directory, fileKeyCodec(directory));
  const placeholders = new Placeholders().add(directory, "<data>");
  const writer = new ConfigWriter(store);
  const cases: Case[] = [];
  for (const [index, [name, input]] of configCases.entries()) {
    const created = store.createProfile(`Config ${index}`);
    const profile = { ...created, ...input.profile };
    store.saveProfile(profile);
    store.setSecret(secretIds.core(profile.id), `core-key-${index}`);
    store.setSecret(
      secretIds.management(profile.id),
      `management-key-${index}`,
    );
    placeholders.add(profile.id, "<profile>");
    cases.push(
      await outcome(`config ${name}`, input, async () => {
        for (const spec of input.accounts) {
          const { disabled, ...accountInput } = spec;
          const account = await apiAccount(accountInput);
          store.saveApiAccount(
            profile.id,
            { ...account, disabled: disabled ?? false },
            accountInput.apiKey,
          );
        }
        const config = await writer.write(profile.id, input.port);
        const text = readFileSync(
          coreConfigPath(directory, profile.id),
          "utf8",
        );
        return {
          yaml: text,
          parsed: parsedYaml(text),
          returned: config,
        };
      }),
    );
  }
  // An unknown profile fails before anything is written.
  cases.push(
    await outcome(
      "config unknown profile",
      { profileId: "missing", port: 1 },
      () => writer.write("missing", 1),
    ),
  );

  // applied(): the config written for the "every protocol" profile against core reports.
  const base = store.createProfile("Applied");
  store.saveProfile({ ...base, ...fillFirst });
  store.setSecret(secretIds.core(base.id), "core-key-applied");
  store.setSecret(secretIds.management(base.id), "management-key-applied");
  const expected = await writer.write(base.id, 9000);
  placeholders.add(base.id, "<profile>");
  for (const [name, reported] of appliedCases)
    cases.push(
      await outcome(
        `applied ${name}`,
        { expected: "<config for an empty fill-first profile>", reported },
        () =>
          applied(
            expected,
            reported === "<expected>" ? copy(expected) : reported,
          ),
      ),
    );
  store.saveApiAccount(
    base.id,
    {
      id: "acct",
      name: "acct",
      provider: "custom",
      baseUrl: "https://x.example/v1",
      prefix: "",
      models: [{ id: "m", protocol: "anthropic" }],
      disabled: false,
    },
    "k",
  );
  const withAccount = await writer.write(base.id, 9000);
  const claudeEntry = withAccount["claude-api-key"][0];
  const reports: Array<[string, Json]> = [
    [
      "account reported",
      { routing: withAccount.routing, "claude-api-key": [{ ...claudeEntry }] },
    ],
    [
      "account empty headers omitted",
      {
        routing: withAccount.routing,
        "claude-api-key": [
          {
            "api-key": "k",
            "base-url": "https://x.example",
            models: [{ name: "m", alias: "m" }],
          },
        ],
      },
    ],
    ["account not yet reported", { routing: withAccount.routing }],
    [
      "account different key",
      {
        routing: withAccount.routing,
        "claude-api-key": [{ ...claudeEntry, "api-key": "other" }],
      },
    ],
    [
      "account extra entry",
      {
        routing: withAccount.routing,
        "claude-api-key": [{ ...claudeEntry }, { ...claudeEntry }],
      },
    ],
  ];
  for (const [name, reported] of reports)
    cases.push(
      await outcome(
        `applied ${name}`,
        { expected: withAccount, reported },
        () => applied(withAccount, reported),
      ),
    );

  // Slugs: accents, punctuation, duplicates, length and empty stems.
  const names = [
    "Work",
    "Work",
    "work",
    "Équipe Café: R&D (été)",
    "★ ★ ★",
    "",
    "   ",
    "profile",
    "Ünïcödé ﬁ ligature",
    "a".repeat(60),
    `${"b".repeat(39)} c`,
    "-leading-and-trailing-",
    "日本語",
    "Ångström 2",
    "x--y__z",
  ];
  mkdirSync(join(directory, "slugs"));
  const slugStore = new Store(
    join(directory, "slugs"),
    fileKeyCodec(join(directory, "slugs")),
  );
  cases.push({
    name: "slugs in creation order",
    input: names,
    output: names.map((n) => slugStore.createProfile(n).slug),
  });
  slugStore.close();
  store.close();
  return placeholders.apply(cases);
}
