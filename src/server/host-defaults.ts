// Plain data shared by the hosts, vite.config.ts and the build scripts; keep it free of imports.

/** Where the desktop and development hosts listen unless NONSTOPVIBIN_PORT says otherwise. */
export const defaultPort = 4318;
export const hostPort = (): number =>
  Number(process.env.NONSTOPVIBIN_PORT || defaultPort);
/** The reviewed core pin installed builds offer; NONSTOPVIBIN_CORE_FEED tries another, such as a pull request's. */
export const coreFeed = (): string =>
  process.env.NONSTOPVIBIN_CORE_FEED ||
  "https://raw.githubusercontent.com/samuelfarkas/nonstopvibin/main/src/server/core-release.json";
/** Vite's development server; the development host accepts its Host and Origin. */
export const devServer = { host: "127.0.0.1", port: 5173 } as const;
export const devHost = `${devServer.host}:${devServer.port}`;
/** The built client, relative to a source checkout or the desktop app. */
export const checkoutLayout = { client: "dist/client" } as const;
/** Signals every host answers by stopping in order. */
export const terminationSignals = ["SIGINT", "SIGTERM", "SIGHUP"] as const;

/**
 * The headless server archive's top-level entries. The CLI finds `core` and `client` beside
 * its real path; scripts/build-server.mjs stages every entry and install.sh copies them.
 */
export const serverArchive = {
  executable: "nonstopvibin",
  client: "client",
  core: "core",
  licenses: "licenses",
  installer: "install.sh",
  readme: "README.md",
} as const;
/** The headless CLI's default `--port`. */
export const serverPort = 4320;
/** The headless CLI's exit code for "no server is running" (`status`, `url`). */
export const notRunningExit = 3;
