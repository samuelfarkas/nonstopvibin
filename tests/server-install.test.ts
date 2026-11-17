import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";

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
