// Run with `bun run test:update:mac` on macOS after `bun run dist:built`: an older build of
// this source updates itself to the packaged release through its own dialog.
// GitHub Actions only: the relaunched app runs without this test's environment, so it opens the default
// data folder, and Squirrel.Mac keeps its state under the real bundle identifier.
import assert from "node:assert/strict";
import { execFile, spawn, spawnSync } from "node:child_process";
import {
  cpSync,
  createReadStream,
  existsSync,
  mkdirSync,
  mkdtempSync,
  openSync,
  readFileSync,
  readdirSync,
  realpathSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { createServer } from "node:http";
import { homedir, tmpdir } from "node:os";
import { basename, join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { promisify } from "node:util";
import YAML from "yaml";
import pkg from "../package.json" with { type: "json" };

assert.equal(process.platform, "darwin", "Run the update test on macOS.");
assert.equal(
  process.env.GITHUB_ACTIONS,
  "true",
  "Run the update test on a GitHub Actions runner: the updated app opens the default data folder.",
);
assert.ok(pkg.updatesEnabled, "The update test needs updatesEnabled.");

const root = resolve(import.meta.dirname, "..");
const release = join(root, "release");
const results = join(root, "test-results/mac-update");
const product = `${pkg.build.productName}.app`;
const requirement = `identifier "${pkg.build.appId}"`;
const oldVersion = "0.0.1";
// ps shows the relaunched app under the resolved path: /private/var, not /var.
const work = realpathSync(mkdtempSync(join(tmpdir(), "nv-mac-update-")));
const appLog = join(work, "app.log");
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
// Empty while Squirrel.Mac swaps the bundle.
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

/** What Squirrel.Mac relies on: a valid deep signature whose requirement is the identifier. */
function checkSignature(app) {
  command("codesign", [
    "--verify",
    "--deep",
    "--strict",
    "--all-architectures",
    `-R=${requirement}`,
    app,
  ]);
  assert.equal(
    command("codesign", ["-d", "-r-", app]).stdout.trim(),
    `designated => ${requirement}`,
  );
  assert.match(command("codesign", ["-dv", app]).stderr, /^Signature=adhoc$/m);
}

async function runningPid(app) {
  const executable = join(app, "Contents/MacOS", pkg.build.productName);
  const { stdout } = await run("ps", ["-ax", "-ww", "-o", "pid=,command="]);
  const line = stdout
    .split("\n")
    .map((entry) => entry.trim())
    .find((entry) =>
      entry.slice(entry.indexOf(" ") + 1).startsWith(executable),
    );
  return line && Number(line.slice(0, line.indexOf(" ")));
}

async function waitFor(what, limit, probe) {
  const deadline = Date.now() + limit;
  for (;;) {
    const value = await probe();
    if (value) return value;
    if (Date.now() > deadline)
      throw new Error(`Timed out waiting for ${what}.`);
    await delay(1000);
  }
}

// UI scripting needs Accessibility access, which GitHub's macOS runners grant osascript.
const clickScript = (pid, button) => `
tell application "System Events"
  set matches to (processes whose unix id is ${pid})
  if matches is {} then return ""
  repeat with w in windows of item 1 of matches
    if exists button "${button}" of w then
      click button "${button}" of w
      return "clicked"
    end if
  end repeat
end tell
return ""`;

const requests = [];
const feed = createServer((request, response) => {
  const name = decodeURIComponent(
    new URL(request.url, "http://feed").pathname.slice(1),
  );
  requests.push(name);
  const file = join(release, name);
  if (name.includes("/") || !existsSync(file)) {
    response.writeHead(404);
    response.end();
    return;
  }
  response.writeHead(200, { "Content-Length": statSync(file).size });
  createReadStream(file).pipe(response);
});
await new Promise((done) => feed.listen(0, "127.0.0.1", done));
const feedUrl = `http://127.0.0.1:${feed.address().port}/`;
let installed;
let exit = 1;
try {
  const update = YAML.parse(
    readFileSync(join(release, "latest-mac.yml"), "utf8"),
  );
  assert.equal(update.version, pkg.version);
  const zip = update.files.find((file) => file.url.endsWith(".zip")).url;
  const shipped = join(work, "shipped");
  command("ditto", ["-x", "-k", join(release, zip), shipped]);
  checkSignature(join(shipped, product));
  console.log(
    `The packaged ${zip} is ad-hoc signed and requires ${requirement}.`,
  );

  const config = join(work, "electron-builder.json");
  writeFileSync(
    config,
    JSON.stringify({
      ...pkg.build,
      extraMetadata: { version: oldVersion },
      publish: [{ provider: "generic", url: feedUrl }],
      directories: { output: join(work, "old") },
      mac: { ...pkg.build.mac, target: ["zip"] },
    }),
  );
  command("bun", ["run", "dist:built", "--mac", "--config", config], {
    cwd: root,
    stdio: "inherit",
  });
  const output = readdirSync(join(work, "old")).find(
    (name) =>
      name.startsWith("mac") && existsSync(join(work, "old", name, product)),
  );
  assert.ok(output, "The older build produced no app.");
  const oldApp = join(work, "old", output, product);
  const folder = join(work, "Applications");
  mkdirSync(folder);
  installed = join(folder, product);
  command("ditto", [oldApp, installed]);
  assert.equal(plistVersion(installed), oldVersion);
  checkSignature(installed);

  const log = openSync(appLog, "a");
  const app = spawn(
    join(installed, "Contents/MacOS", pkg.build.productName),
    ["--background"],
    {
      stdio: ["ignore", log, log],
      env: {
        ...process.env,
        NONSTOPVIBIN_DATA_DIR: join(work, "data"),
        NONSTOPVIBIN_PORT: "0",
      },
    },
  );
  const oldExit = new Promise((done) => app.once("exit", done));
  // The app checks 30 seconds after it starts, then downloads in the background.
  await waitFor("the update dialog", 150_000, async () => {
    if (app.exitCode !== null || app.signalCode)
      throw new Error("The older app exited.");
    const { stdout } = await run("osascript", [
      "-e",
      clickScript(app.pid, "Restart to update"),
    ]);
    return stdout.trim() === "clicked";
  });
  console.log("Accepted the update in the app's dialog.");
  assert.equal(
    await Promise.race([oldExit, delay(60_000, "running")]),
    0,
    "The older app did not quit to install the update.",
  );
  await waitFor(
    "the updated bundle",
    120_000,
    () => plistVersion(installed) === pkg.version,
  );
  const relaunched = await waitFor("the updated app to start", 60_000, () =>
    runningPid(installed),
  );
  checkSignature(installed);
  console.log(
    `Squirrel.Mac replaced ${oldVersion} with ${pkg.version} in place and started it.`,
  );
  process.kill(relaunched, "SIGTERM");
  await waitFor(
    "the updated app to quit",
    30_000,
    async () => !(await runningPid(installed)),
  );
  exit = 0;
} catch (error) {
  console.error(error);
  mkdirSync(results, { recursive: true });
  spawnSync("screencapture", ["-x", join(results, "screen.png")]);
  writeFileSync(join(results, "requests.txt"), requests.join("\n"));
  const shipIt = join(homedir(), `Library/Caches/${pkg.build.appId}.ShipIt`);
  for (const file of [
    appLog,
    join(shipIt, "ShipIt_stderr.log"),
    join(shipIt, "ShipIt_stdout.log"),
  ])
    if (existsSync(file)) cpSync(file, join(results, basename(file)));
  const left = installed && (await runningPid(installed));
  if (left) process.kill(left, "SIGKILL");
} finally {
  feed.close();
  rmSync(work, { recursive: true, force: true });
}
process.exit(exit);
