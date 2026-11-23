// Squirrel.Mac, which installs updates for Electron builds, relaunches the updated app by
// running ShipIt from the new bundle:
//   Contents/Frameworks/Squirrel.framework/Resources/ShipIt ___launch___ <bundle path>
// The Rust app ships this in that place so the update from an Electron build onto it ends with
// the app running again. It answers only that request, as Squirrel.Mac's ShipIt does, through
// Launch Services.
#include <string.h>
#include <unistd.h>

int main(int argc, char **argv) {
  if (argc != 3 || strcmp(argv[1], "___launch___") != 0) return 1;
  execl("/usr/bin/open", "open", argv[2], (char *)0);
  return 1;
}
