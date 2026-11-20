import { createServer } from "node:net";

/**
 * A free loopback port for a process that takes a port number. A port from the system's
 * ephemeral range (from 32768 on Linux, 49152 on macOS), released for the process to bind,
 * can meanwhile be handed to any `listen(0)` or outgoing connection. Below that range a port
 * free now stays free until something binds that number; each is handed out once per run.
 * The range also keeps clear of the app's default ports.
 */
const handedOut = new Set<number>();
export async function unusedPort(): Promise<number> {
  for (;;) {
    const port = 20_000 + Math.floor(Math.random() * 12_000);
    if (handedOut.has(port)) continue;
    const listener = createServer();
    const free = await new Promise<boolean>((done) => {
      listener.once("error", () => done(false));
      listener.listen(port, "127.0.0.1", () =>
        listener.close(() => done(true)),
      );
    });
    if (!free) continue;
    handedOut.add(port);
    return port;
  }
}
