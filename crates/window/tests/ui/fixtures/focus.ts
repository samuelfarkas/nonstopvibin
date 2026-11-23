// Every stylesheet the app ships, the shared one first, and the focus handling itself.
import "../../../../../src/client/styles.css";
import.meta.glob("../../../../../src/client/*.css", { eager: true });
import "../../../../../src/client/focus.ts";
