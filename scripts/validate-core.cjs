const { join } = require("node:path");
const { Arch } = require("electron-builder");
const {
  checkCoreDirectory,
  coreLayout,
} = require("../src/server/core-binary.ts");

module.exports = async function validateCore(context) {
  checkCoreDirectory(
    join(context.packager.projectDir, coreLayout.checkout),
    context.electronPlatformName,
    Arch[context.arch],
  );
};
