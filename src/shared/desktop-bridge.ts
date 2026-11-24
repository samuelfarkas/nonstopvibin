// `window.nonstopvibin`: the privileged calls the desktop app offers its own pages. The
// helper's bridge script defines one method per name of `Method::ALL`
// (crates/desktop/src/bridge.rs), which `bun run wire:export` writes to ./generated/desktop.

import type { BridgeMethod } from "./generated/desktop/BridgeMethod.ts";

/** Every method takes at most one argument, which the daemon validates. */
interface Methods {
  bootstrap(): Promise<{ token: string }>;
  openExternal(url: string): Promise<void>;
  copy(text: string): Promise<void>;
  showWindow(profileId?: string): Promise<void>;
  releaseWindow(): Promise<void>;
  setLoginItem(enabled: boolean): Promise<boolean>;
  getLoginItem(): Promise<boolean>;
  revealData(): Promise<void>;
  chooseProject(): Promise<string | null>;
}
/** A missing method fails to index `Methods`, and an extra one is not on the bridge. */
export type DesktopBridge = { [M in BridgeMethod]: Methods[M] };

declare global {
  interface Window {
    nonstopvibin?: DesktopBridge;
  }
}
