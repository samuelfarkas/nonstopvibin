import { readdir, readFile, lstat } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";
import { createHash } from "node:crypto";
import type { ImportSource } from "../shared/types.ts";
import { parse, record, text, type JsonObject } from "./json.ts";
import { AppError } from "./errors.ts";
import { claudeIdentity } from "./identity.ts";
import { isCredentialFile } from "../shared/providers.ts";
import { credentialFileBytes } from "../shared/vocabulary.ts";

// Only known local installations are scanned. The renderer never supplies a filesystem path.
export class ImportCatalog {
  private files = new Map<string, string>();
  readonly directories: string[];
  constructor(
    directories = [
      join(homedir(), "Library/Application Support/com.cpa.gui/oauth"),
      join(homedir(), ".local/share/com.cpa.gui/oauth"),
      join(homedir(), ".cli-proxy-api"),
    ],
  ) {
    this.directories = directories;
  }
  async scan(): Promise<ImportSource[]> {
    const result: ImportSource[] = [];
    this.files.clear();
    for (const directory of this.directories) {
      let names;
      try {
        names = await readdir(directory, { withFileTypes: true });
      } catch {
        continue;
      }
      for (const entry of names) {
        if (!entry.isFile() || !entry.name.endsWith(".json")) continue;
        const path = join(directory, entry.name);
        try {
          const raw = await this.read(path);
          if (!isCredentialFile(String(raw.type))) continue;
          const id = createHash("sha256").update(path).digest("hex");
          this.files.set(id, path);
          result.push({
            id,
            fileName: entry.name,
            provider: String(raw.type),
            email: text(raw.email),
            ...(raw.type === "claude" && claudeIdentity(raw)),
            source: directory.includes("com.cpa.gui")
              ? "EasyCLIProxyAPI"
              : "CLIProxyAPI",
          });
        } catch {
          /* Skip unrelated, unreadable, or malformed files without exposing their contents. */
        }
      }
    }
    return result;
  }
  private async read(path: string): Promise<JsonObject> {
    const file = await lstat(path);
    if (!file.isFile() || file.size > credentialFileBytes)
      throw new AppError(
        "The selected account file is not a supported regular file.",
      );
    return record(parse(await readFile(path, "utf8")));
  }
  async contents(id: string): Promise<JsonObject> {
    const path = this.files.get(id);
    if (!path)
      throw new AppError(
        "Refresh the detected accounts and select the file again.",
        404,
      );
    return this.read(path);
  }
}
