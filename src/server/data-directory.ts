import { chmodSync, mkdirSync } from "node:fs";
import { homedir } from "node:os";
import { join, resolve } from "node:path";

// The layout of the data directory. Installed apps already hold data under these
// exact names: a rename here strands the owner's profiles, keys and credentials.

export function dataDirectory(override = process.env.NONSTOPVIBIN_DATA_DIR) {
  return resolve(override || join(homedir(), ".nonstopvibin"));
}
/**
 * Creates the directory owner-only and tightens an existing one. `check` vets it after
 * creation and before the mode changes, since chmod would follow a symbolic link.
 */
export function ownerOnlyDirectory(directory: string, check?: () => void) {
  mkdirSync(directory, { recursive: true, mode: 0o700 });
  check?.();
  chmodSync(directory, 0o700);
}
export const databasePath = (directory: string) =>
  join(directory, "nonstopvibin.sqlite");
export const vaultKeyPath = (directory: string) => join(directory, "vault.key");
export const profileDirectory = (directory: string, profileId: string) =>
  join(directory, "profiles", profileId);
export const credentialDirectory = (directory: string, profileId: string) =>
  join(profileDirectory(directory, profileId), "auth");
export const coreConfigPath = (directory: string, profileId: string) =>
  join(profileDirectory(directory, profileId), "config.yaml");
/** The core's arguments; reaping orphaned cores recognises them by this command line. */
export const coreArguments = (directory: string, profileId: string) => [
  "--config",
  coreConfigPath(directory, profileId),
];
/** Throwaway data directories for Claude sign-ins awaiting review and core update checks. */
export const oauthPendingDirectory = (directory: string) =>
  join(directory, "oauth-pending");
/** Core updates installed builds downloaded, as `<version>/`; see core-updates.ts. */
export const coresDirectory = (directory: string) => join(directory, "cores");
/** Ledger of running cores, for the next instance to reap after a crash. */
export const coreLedgerDirectory = (directory: string) =>
  join(directory, "core-processes");
export const agentConnectionsDirectory = (directory: string) =>
  join(directory, "agent-connections");
/** The headless server's discovery file. */
export const sessionPath = (directory: string) =>
  join(directory, "server.json");
/** The headless server's single-instance lock. */
export const serverLockPath = (directory: string) =>
  join(directory, "server.lock.sqlite");
