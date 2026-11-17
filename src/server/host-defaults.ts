// Plain data shared by the hosts and vite.config.ts; keep it free of imports.

/** Where the desktop and development hosts listen unless NONSTOPVIBIN_PORT says otherwise. */
export const defaultPort = 4318;
export const hostPort = (): number =>
  Number(process.env.NONSTOPVIBIN_PORT || defaultPort);
/** Vite's development server; the development host accepts its Host and Origin. */
export const devServer = { host: "127.0.0.1", port: 5173 } as const;
export const devHost = `${devServer.host}:${devServer.port}`;
/** The core binary and the built client, relative to a source checkout or the desktop app. */
export const checkoutLayout = {
  core: ".vendor/core/cli-proxy-api",
  client: "dist/client",
} as const;
/** Signals every host answers by stopping in order. */
export const terminationSignals = ["SIGINT", "SIGTERM", "SIGHUP"] as const;
