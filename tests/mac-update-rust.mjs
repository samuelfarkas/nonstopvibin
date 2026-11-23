// Run with `bun run test:update:mac` on a GitHub Actions macOS runner after
// `bun scripts/package-mac.ts`: an installed Rust build updates itself in place to the packaged
// release. It builds this checkout again as 0.0.1 with a test feed on 127.0.0.1
// (NONSTOPVIBIN_TEST_UPDATE_FEED) and a throwaway release key, serves the packaged release with a
// SHA256SUMS signed by that key, and checks that the old app refuses a bundle signed with another
// identifier and a SHA256SUMS signed by another key, then installs the real release through
// "Restart to update" and relaunches it from the same path, on the same data folder.
import assert from "node:assert/strict";
import { execFile, spawn, spawnSync } from "node:child_process";
import {
  createHash,
  generateKeyPairSync,
  randomBytes,
  sign,
} from "node:crypto";
import {
  copyFileSync,
  cpSync,
  createReadStream,
  existsSync,
  mkdirSync,
  mkdtempSync,
  openSync,
  readFileSync,
  realpathSync,
  rmSync,
  statSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { promisify } from "node:util";
import YAML from "yaml";
import pkg from "../package.json" with { type: "json" };
import { product } from "../scripts/distribution.ts";
import { trustedComment, writeSums } from "../scripts/sign-release.ts";

assert.equal(process.platform, "darwin", "Run the update test on macOS.");
assert.equal(
  process.env.GITHUB_ACTIONS,
  "true",
  "Run the update test on a GitHub Actions runner: osascript needs its Accessibility grant.",
);

const root = resolve(import.meta.dirname, "..");
const release = join(root, "release");
const results = join(root, "test-results/mac-update-rust");
const bundle = `${product.name}.app`;
const version = pkg.version;
const old = "0.0.1";
const port = 48731;
const work = realpathSync(mkdtempSync(join(tmpdir(), "nv-mac-rust-update-")));
const apps = join(work, "Applications");
const installed = join(apps, bundle);
const executable = join(installed, "Contents/MacOS", product.name);
const appLog = join(work, "app.log");
const env = {
  ...process.env,
  NONSTOPVIBIN_DATA_DIR: join(work, "data"),
  NONSTOPVIBIN_PORT: "0",
};
const run = promisify(execFile);

function command(file, args, options = {}) {
  const result = spawnSync(file, args, { encoding: "utf8", ...options });
  assert.equal(
    result.status,
    0,
    `${file} ${args.join(" ")} failed:\n${result.stdout ?? ""}${result.stderr ?? ""}`,
  );
  return result;
}
const plistVersion = (app) =>
  spawnSync(
    "plutil",
    [
      "-extract",
      "CFBundleShortVersionString",
      "raw",
      join(app, "Contents/Info.plist"),
    ],
    { encoding: "utf8" },
  ).stdout.trim();
async function waitFor(what, limit, probe) {
  const deadline = Date.now() + limit;
  for (;;) {
    const value = await probe();
    if (value) return value;
    if (Date.now() > deadline)
      throw new Error(`Timed out waiting for ${what}.`);
    await delay(500);
  }
}
async function pidOf(path) {
  const { stdout } = await run("ps", ["-ax", "-ww", "-o", "pid=,command="]);
  const line = stdout
    .split("\n")
    .map((entry) => entry.trim())
    .find((entry) => entry.slice(entry.indexOf(" ") + 1).startsWith(path));
  return line && Number(line.slice(0, line.indexOf(" ")));
}

// minisign's prehashed Ed25519 format, which scripts/sign-release.ts produces with minisign
// (no minisign binary is pinned for macOS).
const keyId = randomBytes(8);
const { privateKey, publicKey } = generateKeyPairSync("ed25519");
const rawPublic = Buffer.from(
  publicKey.export({ format: "jwk" }).x,
  "base64url",
);
/** Another key, whose signatures the old app must refuse. */
const foreign = { keyId: randomBytes(8), ...generateKeyPairSync("ed25519") };
function minisign(file, key = { keyId, privateKey }) {
  const message = createHash("blake2b512").update(readFileSync(file)).digest();
  const signature = sign(null, message, key.privateKey);
  const trusted = trustedComment(version);
  const global = sign(
    null,
    Buffer.concat([signature, Buffer.from(trusted)]),
    key.privateKey,
  );
  writeFileSync(
    `${file}.minisig`,
    `untrusted comment: test signature\n${Buffer.concat([Buffer.from("ED"), key.keyId, signature]).toString("base64")}\ntrusted comment: ${trusted}\n${global.toString("base64")}\n`,
  );
}

const files = new Map();
const requests = [];
const feed = createServer((request, response) => {
  const path = decodeURIComponent(
    new URL(request.url, "http://feed").pathname.slice(1),
  );
  requests.push(path);
  const file = files.get(path);
  if (!file) {
    response.writeHead(404);
    response.end();
    return;
  }
  response.writeHead(200, { "Content-Length": statSync(file).size });
  createReadStream(file).pipe(response);
});
/** Serves `directory` as the release `version` of this repository. */
function publish(directory, names) {
  files.clear();
  const atom = join(work, "releases.atom");
  writeFileSync(
    atom,
    `<feed><entry><link rel="alternate" type="text/html" href="${pkg.homepage}/releases/tag/v${version}"/></entry></feed>`,
  );
  files.set("releases.atom", atom);
  for (const name of [...names, "SHA256SUMS", "SHA256SUMS.minisig"])
    files.set(`releases/download/v${version}/${name}`, join(directory, name));
}
/** SHA256SUMS as the release writes it, signed with the test key or `key`. */
async function sums(directory, names, key) {
  await writeSums(directory, names);
  minisign(join(directory, "SHA256SUMS"), key);
}

let app;
let exit = 1;
await new Promise((done) => feed.listen(port, "127.0.0.1", done));
try {
  // The old version: this checkout at 0.0.1, pointed at the test feed, with the test key.
  const source = join(work, "src");
  const skipped = new Set(
    ["target", "node_modules", ".git", "release", "test-results"].map((name) =>
      join(root, name),
    ),
  );
  cpSync(root, source, {
    recursive: true,
    verbatimSymlinks: true,
    filter: (path) => !skipped.has(path),
  });
  symlinkSync(join(root, "node_modules"), join(source, "node_modules"));
  writeFileSync(
    join(source, "build/update-signing.pub"),
    `untrusted comment: test key\n${Buffer.concat([Buffer.from("Ed"), keyId, rawPublic]).toString("base64")}\n`,
  );
  const rewrite = (file, pattern, replacement) => {
    const text = readFileSync(join(source, file), "utf8");
    assert.match(text, pattern, file);
    writeFileSync(join(source, file), text.replace(pattern, replacement));
  };
  rewrite("package.json", /^ {2}"version": ".*",$/m, `  "version": "${old}",`);
  rewrite(
    "Cargo.toml",
    /^(\[workspace\.package\]\nversion = )".*"/m,
    `$1"${old}"`,
  );
  rewrite(
    "Cargo.lock",
    /^(name = "nsv-[a-z]+"\nversion = )".*"/gm,
    `$1"${old}"`,
  );
  command("bun", ["scripts/package-mac.ts"], {
    cwd: source,
    stdio: "inherit",
    env: {
      ...process.env,
      CARGO_TARGET_DIR: process.env.CARGO_TARGET_DIR ?? join(root, "target"),
      NONSTOPVIBIN_TEST_UPDATE_FEED: `http://127.0.0.1:${port}/`,
    },
  });
  mkdirSync(apps);
  command("ditto", [
    "-x",
    "-k",
    join(source, "release", `${product.name}-${old}-mac-arm64.zip`),
    apps,
  ]);
  assert.equal(plistVersion(installed), old);

  const launch = () => {
    writeFileSync(appLog, "");
    const log = openSync(appLog, "a");
    app = spawn(executable, ["--background"], {
      stdio: ["ignore", log, log],
      env,
    });
    return new Promise((done) => app.once("exit", (code) => done(code)));
  };
  const logged = (text) => readFileSync(appLog, "utf8").includes(text);
  const stop = async (exited) => {
    app.kill("SIGTERM");
    await exited;
  };
  const update = YAML.parse(
    readFileSync(join(release, "latest-mac.yml"), "utf8"),
  );
  assert.equal(update.version, version);
  const zip = update.files.find((file) => file.url.endsWith(".zip")).url;

  // A bundle with another identifier fails the running app's designated requirement.
  const forged = join(work, "forged");
  command("ditto", ["-x", "-k", join(release, zip), join(forged, "app")]);
  command("codesign", [
    "--force",
    "--sign",
    "-",
    "-i",
    "com.example.forged",
    join(forged, "app", bundle),
  ]);
  command("ditto", [
    "-c",
    "-k",
    "--sequesterRsrc",
    "--keepParent",
    join(forged, "app", bundle),
    join(forged, zip),
  ]);
  const bytes = readFileSync(join(forged, zip));
  writeFileSync(
    join(forged, "latest-mac.yml"),
    YAML.stringify(
      {
        ...update,
        files: [
          {
            url: zip,
            sha512: createHash("sha512").update(bytes).digest("base64"),
            size: bytes.length,
          },
        ],
      },
      { version: "1.1" },
    ),
  );
  await sums(forged, [zip, "latest-mac.yml"]);
  publish(forged, [zip, "latest-mac.yml"]);
  let exited = launch();
  await waitFor("the forged bundle to be refused", 90_000, () =>
    logged("could not verify the signature of the update"),
  );
  await stop(exited);
  assert.equal(plistVersion(installed), old);
  console.log(
    "A bundle signed with another identifier is refused; the app is unchanged.",
  );

  // The packaged release, first signed by another key.
  const real = join(work, "release");
  mkdirSync(real);
  for (const name of [zip, "latest-mac.yml"])
    copyFileSync(join(release, name), join(real, name));
  await sums(real, [zip, "latest-mac.yml"], foreign);
  publish(real, [zip, "latest-mac.yml"]);
  exited = launch();
  await waitFor("the foreign signature to be refused", 90_000, () =>
    logged("SHA256SUMS is not signed by the release key"),
  );
  await stop(exited);
  assert.equal(plistVersion(installed), old);
  console.log(
    "SHA256SUMS signed by another key is refused; the app is unchanged.",
  );

  await sums(real, [zip, "latest-mac.yml"]);
  exited = launch();
  const helper = join(installed, "Contents/MacOS/nonstopvibin-window");
  // UI scripting needs Accessibility access, which GitHub's macOS runners grant osascript.
  await waitFor("the update dialog", 150_000, async () => {
    if (app.exitCode !== null) throw new Error("The old app exited.");
    const pid = await pidOf(helper);
    if (!pid) return false;
    const { stdout } = await run("osascript", [
      "-e",
      `tell application "System Events"
  repeat with w in windows of (first process whose unix id is ${pid})
    if exists button "Restart to update" of w then
      click button "Restart to update" of w
      return "clicked"
    end if
  end repeat
end tell
return ""`,
    ]).catch(() => ({ stdout: "" }));
    return stdout.trim() === "clicked";
  });
  console.log("Accepted the update in the app's dialog.");
  assert.equal(
    await Promise.race([exited, delay(60_000, "running")]),
    0,
    "The old app did not quit with 0.",
  );
  assert.equal(plistVersion(installed), version);
  command("codesign", ["--verify", "--deep", "--strict", installed]);
  const relaunched = await waitFor("the updated app to start", 60_000, () =>
    pidOf(executable),
  );
  const state = await waitFor(
    "the updated app to serve the data folder",
    60_000,
    () => {
      const result = spawnSync(executable, ["url", "--json"], {
        env,
        encoding: "utf8",
      });
      return result.status === 0 && JSON.parse(result.stdout);
    },
  );
  assert.equal(state.version, version);
  assert.equal(state.pid, relaunched);
  await waitFor(
    "the staging folder to be removed",
    30_000,
    () => !existsSync(join(apps, `.${bundle}.update`)),
  );
  assert.ok(
    !logged("app update failed"),
    "the accepted update logged a failure",
  );
  console.log(
    `${old} replaced itself with ${version} in place and relaunched it as pid ${relaunched}.`,
  );
  process.kill(relaunched, "SIGTERM");
  await waitFor(
    "the updated app to quit",
    30_000,
    async () => !(await pidOf(executable)),
  );
  exit = 0;
} catch (error) {
  console.error(error);
  mkdirSync(results, { recursive: true });
  spawnSync("screencapture", ["-x", join(results, "screen.png")]);
  writeFileSync(join(results, "requests.txt"), requests.join("\n"));
  if (existsSync(appLog)) cpSync(appLog, join(results, "app.log"));
  for (const path of [
    executable,
    join(installed, "Contents/MacOS/nonstopvibin-window"),
  ]) {
    const left = await pidOf(path);
    if (left) process.kill(left, "SIGKILL");
  }
  app?.kill("SIGKILL");
} finally {
  feed.close();
  rmSync(work, { recursive: true, force: true });
}
process.exit(exit);
