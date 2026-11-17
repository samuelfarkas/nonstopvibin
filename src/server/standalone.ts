import { Application } from "./server.ts";
import { dataDirectory } from "./data-directory.ts";
import { devHost } from "./host-defaults.ts";
import { runHost } from "./host.ts";

const application = await runHost((checkout, signal) =>
  Application.create(
    {
      directory: dataDirectory(),
      ...checkout,
      development: true,
      allowMissingCore: true,
    },
    signal,
  ),
);
console.log(`nonstopvibin API: ${application.origin}`);
console.log(
  `Open the local interface: http://${devHost}/#session=${application.token}`,
);
