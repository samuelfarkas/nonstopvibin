// The privileged calls the desktop shell offers the renderer. The preload script
// builds `window.nonstopvibin` from `bridgeChannels`; the main process registers
// one validated handler per method, typed against `DesktopBridge`. Types and
// strings only: the sandboxed preload bundles this module.

/** Every method takes at most one argument, which main validates. */
export interface DesktopBridge {
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

declare global {
  interface Window {
    nonstopvibin?: DesktopBridge;
  }
}
