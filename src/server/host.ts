import { join, resolve } from "node:path";
import type { Application } from "./server.ts";
import { coreLayout } from "./core-binary.ts";
import {
  checkoutLayout,
  hostPort,
  terminationSignals,
} from "./host-defaults.ts";

/** A host's core binary, client directory and port when it runs from this source checkout. */
export interface HostDefaults {
  binary: string;
  clientDirectory: string;
  port: number;
}

/**
 * Runs an application as this process. A termination signal stops it and exits 0; an uncaught
 * exception or unhandled rejection is printed, stops it and exits 1; a failed stop is printed
 * and exits 1. A signal or crash during `start` cancels it through `signal` and exits the same
 * way. Further signals while stopping are ignored.
 */
export async function runHost(
  start: (defaults: HostDefaults, signal: AbortSignal) => Promise<Application>,
): Promise<Application> {
  const root = resolve(import.meta.dirname, "../..");
  const controller = new AbortController();
  let application: Application | undefined;
  let stopping = false;
  let code = 0;
  const stop = () => {
    controller.abort();
    if (!application || stopping) return;
    stopping = true;
    application.close().then(
      () => process.exit(code),
      (error) => {
        console.error(error);
        process.exit(1);
      },
    );
  };
  const crashed = () => {
    code = 1;
    stop();
  };
  const uncaught: NodeJS.UncaughtExceptionListener = (error) => {
    console.error(error);
    crashed();
  };
  const unhandled: NodeJS.UnhandledRejectionListener = (reason) => {
    console.error(reason);
    crashed();
  };
  // Receive termination before any core spawns; cancelling startup awaits its cleanup.
  for (const signal of terminationSignals) process.on(signal, stop);
  // A crash must still stop the profile cores; the watchdogs cover what this cannot.
  process.on("uncaughtException", uncaught);
  process.on("unhandledRejection", unhandled);
  try {
    application = await start(
      {
        binary: join(root, coreLayout.checkout, coreLayout.binary),
        clientDirectory: join(root, checkoutLayout.client),
        port: hostPort(),
      },
      controller.signal,
    );
  } catch (error) {
    for (const signal of terminationSignals)
      process.removeListener(signal, stop);
    process.removeListener("uncaughtException", uncaught);
    process.removeListener("unhandledRejection", unhandled);
    if (error === controller.signal.reason) process.exit(code);
    throw error;
  }
  // A start that finished despite being cancelled stops now.
  if (controller.signal.aborted) stop();
  return application;
}
