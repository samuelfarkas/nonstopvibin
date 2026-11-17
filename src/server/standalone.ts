import { resolve } from "node:path";
import { Application } from "./server.ts";
import { dataDirectory } from "./data-directory.ts";

const application = await Application.create({
  directory: dataDirectory(),
  binary: resolve(".vendor/core/cli-proxy-api"),
  clientDirectory: resolve("dist/client"),
  port: Number(process.env.NONSTOPVIBIN_PORT || 4318),
  development: true,
});
console.log(`nonstopvibin API: ${application.origin}`);
console.log(
  `Open the local interface: http://127.0.0.1:5173/#session=${application.token}`,
);
let closing = false;
function shutdown(code: number): void {
  if (closing) return;
  closing = true;
  application.close().then(
    () => process.exit(code),
    (error) => {
      console.error(error);
      process.exit(1);
    },
  );
}
for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"])
  process.on(signal, () => shutdown(0));
// A crash must still stop the profile cores; the watchdogs cover what this cannot.
for (const event of ["uncaughtException", "unhandledRejection"])
  process.on(event, (error) => {
    console.error(error);
    shutdown(1);
  });
