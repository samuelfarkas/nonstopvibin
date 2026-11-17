import { randomBytes, createCipheriv, createDecipheriv } from "node:crypto";
import { readFileSync, writeFileSync, chmodSync, existsSync } from "node:fs";
import { databasePath, vaultKeyPath } from "./data-directory.ts";

export interface SecretCodec {
  encrypt(value: string): string;
  decrypt(value: string): string;
}
// Desktop and development share an owner-only local encryption key.
export function fileKeyCodec(directory: string): SecretCodec {
  const path = vaultKeyPath(directory);
  if (!existsSync(path)) {
    if (existsSync(databasePath(directory)))
      throw new Error(
        "vault.key is missing for an existing database. Restore its matching key from your backup.",
      );
    writeFileSync(path, randomBytes(32), { mode: 0o600, flag: "wx" });
  }
  chmodSync(path, 0o600);
  const key = readFileSync(path);
  if (key.length !== 32)
    throw new Error(
      "The local vault key is invalid. Restore vault.key from your backup.",
    );
  return {
    encrypt(value) {
      const nonce = randomBytes(12);
      const cipher = createCipheriv("aes-256-gcm", key, nonce);
      const encrypted = Buffer.concat([
        cipher.update(value, "utf8"),
        cipher.final(),
      ]);
      return Buffer.concat([nonce, cipher.getAuthTag(), encrypted]).toString(
        "base64",
      );
    },
    decrypt(value) {
      const bytes = Buffer.from(value, "base64");
      const decipher = createDecipheriv(
        "aes-256-gcm",
        key,
        bytes.subarray(0, 12),
      );
      decipher.setAuthTag(bytes.subarray(12, 28));
      return Buffer.concat([
        decipher.update(bytes.subarray(28)),
        decipher.final(),
      ]).toString("utf8");
    },
  };
}
