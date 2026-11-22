// Run with `bun run test:update:mac` on macOS after `bun run dist:built`. Checks the ad-hoc
// signature of the packaged zip, then installs an older build of the same source, serves
// release/ as its update feed, accepts the update in the app's own dialog, and checks that
// Squirrel.Mac replaced the bundle in place and started the new version.
// CI only: the relaunched app runs without this test's environment, so it opens the default
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
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { createServer } from "node:http";
import { homedir, tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { promisify } from "node:util";
import YAML from "yaml";
import pkg from "../package.json" with { type: "json" };

assert.equal(process.platform, "darwin", "Run the update test on macOS.");
assert.ok(
  process.env.CI,
  "Run the update test in CI: the updated app opens the default data folder.",
);

const root = resolve(import.meta.dirname, "..");
const release = join(root, "release");
const results = join(root, "test-results/mac-update");
const product = `${pkg.build.productName}.app`;
const requirement = `identifier "${pkg.build.appId}"`;
const oldVersion = "0.0.1";
const work = mkdtempSync(join(tmpdir(), "nv-mac-update-"));
const appLog = join(work, "app.log");
const run = promisify(execFile);

function command(file, args, options = {}) {
  const result = spawnSync(file, args, { encoding: "utf8", ...options });
  assert.equal(
    result.status,
    0,
    `${file} ${args.join(" ")} failed:\n${result.stdout}${result.stderr}`,
  );
  return result;
}
const plistVersion = (app) =>
  command("plutil", [
    "-extract",
    "CFBundleShortVersionString",
    "raw",
    join(app, "Contents/Info.plist"),
  ]).stdout.trim();

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

/** The pid of a running main executable of `app`, not one of its helpers. */
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

// Clicks a button of the app's native update dialog through the Accessibility API.
const clickScript = (pid, button) => `
tell application "System Events"
  set matches to (processes whose unix id is ${pid})
  if matches is {} then return "gone"
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
let oldApp;
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

  // The same source as an older version whose feed is this server.
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
  command(
    "bunx",
    ["--no-install", "electron-builder", "--mac", "--publish", "never"].concat([
      "--config",
      config,
    ]),
    // electron-builder skips signing in pull requests unless told otherwise.
    {
      cwd: root,
      stdio: "inherit",
      env: { ...process.env, CSC_FOR_PULL_REQUEST: "true" },
    },
  );
  const output = readdirSync(join(work, "old")).find(
    (name) =>
      name.startsWith("mac") && existsSync(join(work, "old", name, product)),
  );
  assert.ok(output, "The older build produced no app.");
  oldApp = join(work, "old", output, product);
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
  const clicked = await waitFor("the update dialog", 150_000, async () => {
    const { stdout } = await run("osascript", [
      "-e",
      clickScript(app.pid, "Restart to update"),
    ]);
    if (stdout.trim() === "gone") throw new Error("The older app exited.");
    return stdout.trim() === "clicked";
  });
  assert.ok(clicked);
  console.log("Accepted the update in the app's dialog.");
  assert.equal(
    await Promise.race([oldExit, delay(60_000, "running")]),
    0,
    "The older app did not quit to install the update.",
  );
  await waitFor(
    "the updated bundle",
    120_000,
    () =>
      existsSync(join(installed, "Contents/Info.plist")) &&
      plistVersion(installed) === pkg.version,
  );
  const relaunched = await waitFor("the updated app to start", 60_000, () =>
    runningPid(installed),
  );
  checkSignature(installed);
  assert.equal(
    spawnSync("xattr", ["-p", "com.apple.quarantine", installed]).status,
    1,
    "The updated app is quarantined.",
  );
  assert.ok(
    requests.includes("latest-mac.yml"),
    "The app never read the feed.",
  );
  assert.ok(requests.includes(zip), "The app never downloaded the update.");
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
    if (existsSync(file)) cpSync(file, join(results, file.split("/").pop()));
  for (const app of [installed, oldApp])
    if (app && existsSync(app)) {
      const pid = await runningPid(app);
      if (pid) process.kill(pid, "SIGKILL");
    }
} finally {
  feed.close();
  rmSync(work, { recursive: true, force: true });
}
process.exit(exit);
