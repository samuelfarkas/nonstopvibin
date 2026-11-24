// `bun scripts/verify-rust-package.ts <artifact>...`: runs scripts/package-layout.ts's checks on
// built .tar.gz, .AppImage, .deb or (on macOS) .zip files.
import { verifyArtifact } from "./package-layout.ts";

const files = process.argv.slice(2);
if (!files.length)
  throw new Error("Usage: bun scripts/verify-rust-package.ts <artifact>...");
for (const file of files) verifyArtifact(file);
