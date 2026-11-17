import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { renderInstaller } from "../scripts/distribution.ts";

test("installer recognizes RPM/openSUSE/Arch trust stores and explicit overrides without asserting TLS validity", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nv-installer-ca-"));
  const source = await readFile("scripts/server-install.sh", "utf8");
  // Execute the real presence check, relocating only filesystem paths so this
  // test neither reads nor modifies the host's system certificate store.
  const functions = source
    .slice(0, source.indexOf("\nusage() {"))
    .replaceAll("/etc/", `${directory}/etc/`);
  const check = (file = "", dirs = "") => {
    try {
      execFileSync("sh", ["-c", `${functions}\nhas_ca_certificates`], {
        env: {
          ...process.env,
          HOME: directory,
          SSL_CERT_FILE: file,
          SSL_CERT_DIR: dirs,
        },
        stdio: "pipe",
      });
      return true;
    } catch (error) {
      if (!(error instanceof Error && "status" in error && error.status === 1))
        throw error;
      return false;
    }
  };
  const pem =
    "-----BEGIN CERTIFICATE-----\npresence-only fixture\n-----END CERTIFICATE-----\n";
  try {
    assert.equal(check(), false);
    for (const path of [
      "pki/tls/certs/ca-bundle.crt",
      "ssl/ca-bundle.pem",
      "ssl/certs/ca-certificates.crt",
      "pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
    ]) {
      const bundle = join(directory, "etc", path);
      await mkdir(dirname(bundle), { recursive: true });
      await writeFile(bundle, "not a certificate");
      assert.equal(check(), false, path);
      await writeFile(bundle, pem);
      assert.equal(check(), true, path);
      await rm(bundle);
    }
    const custom = join(directory, "custom bundle.pem");
    await writeFile(custom, pem);
    assert.equal(check(custom), true);
    assert.equal(check(join(directory, "missing")), false);
    const certificates = join(directory, "custom roots");
    await mkdir(certificates);
    assert.equal(
      check("", certificates),
      false,
      "an empty directory is not a trust bundle",
    );
    await writeFile(join(certificates, "root.pem"), pem);
    assert.equal(check("", `${directory}/missing:${certificates}`), true);
    await rm(join(certificates, "root.pem"));
    await writeFile(join(certificates, ".root.pem"), pem);
    assert.equal(
      check(join(directory, "missing"), certificates),
      true,
      "a directory containing only a hidden PEM file is recognized",
    );
    assert.equal(
      check("", `${directory}/custom*`),
      false,
      "directory overrides are literal, not shell patterns",
    );
    const system = join(directory, "etc/ssl/certs/ca-certificates.crt");
    await writeFile(system, pem);
    assert.equal(
      check(join(directory, "missing"), `${directory}/missing`),
      false,
      "explicit overrides replace system locations",
    );
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("the built installer asks the CLI for its unit, restarts that unit and refuses another executable's", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nv-installer-"));
  const source = join(directory, "archive");
  const bin = join(directory, "bin");
  const calls = join(directory, "calls");
  const unit = join(directory, "config/systemd/user/nonstopvibin.service");
  const script = (path: string, body: string) =>
    writeFile(path, `#!/bin/sh\n${body}\n`, { mode: 0o755 });
  try {
    await mkdir(bin);
    for (const entry of ["client", "core", "licenses"])
      await mkdir(join(source, entry), { recursive: true });
    await writeFile(
      join(source, "install.sh"),
      renderInstaller(
        await readFile("scripts/server-install.sh", "utf8"),
        "0.0.0",
        "x64",
      ),
      { mode: 0o755 },
    );
    // A stand-in CLI: UNIT_STATE says what `service unit` finds.
    await script(
      join(source, "nonstopvibin"),
      `echo "cli $*" >> "${calls}"
case "$1 $2" in
  "--version ") echo "nonstopvibin 0.0.0" ;;
  "service unit")
    case "$UNIT_STATE" in
      same) echo "${unit}" ;;
      other) echo "The existing service uses another executable." >&2; exit 1 ;;
    esac ;;
esac`,
    );
    await script(
      join(bin, "uname"),
      `case "$1" in -s) echo Linux ;; -m) echo "$MACHINE" ;; esac`,
    );
    await script(join(bin, "systemctl"), `echo "systemctl $*" >> "${calls}"`);
    await script(join(bin, "curl"), "exit 0");
    const install = (state: string, machine = "x86_64") => {
      const prefix = join(directory, `prefix-${state}-${machine}`);
      const result = spawnSync(
        "sh",
        [join(source, "install.sh"), "--prefix", prefix],
        {
          encoding: "utf8",
          env: {
            ...process.env,
            HOME: directory,
            PATH: `${bin}:${process.env.PATH}`,
            XDG_RUNTIME_DIR: directory,
            SSL_CERT_FILE: "",
            UNIT_STATE: state,
            MACHINE: machine,
          },
        },
      );
      return { ...result, prefix };
    };
    const log = async () => {
      const text = await readFile(calls, "utf8").catch(() => "");
      await rm(calls, { force: true });
      return text;
    };

    const first = install("none");
    assert.equal(first.status, 0, first.stderr);
    const fresh = await log();
    assert.match(
      fresh,
      /^cli service unit .*\/lib\/nonstopvibin\/nonstopvibin$/m,
    );
    assert.match(fresh, /^cli service install$/m);
    assert.doesNotMatch(fresh, /restart/);

    await mkdir(dirname(unit), { recursive: true });
    await writeFile(unit, "");
    const upgrade = install("same");
    assert.equal(upgrade.status, 0, upgrade.stderr);
    const restarted = await log();
    assert.match(
      restarted,
      /^systemctl --user restart nonstopvibin\.service$/m,
    );
    assert.doesNotMatch(restarted, /service install/);

    const refused = install("other");
    assert.equal(refused.status, 1);
    assert.match(refused.stderr, /another executable/);
    assert.equal(existsSync(join(refused.prefix, "lib")), false);
    await log();

    const arm = install("none", "aarch64");
    assert.equal(arm.status, 1);
    assert.match(
      arm.stderr,
      /this machine is arm64\. Download the linux-arm64/,
    );
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});
