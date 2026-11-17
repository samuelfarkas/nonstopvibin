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
export type BridgeMethod = keyof DesktopBridge;
/** What the renderer sends; undefined for a method without an argument. */
export type BridgeArgument<M extends BridgeMethod> = Parameters<
  DesktopBridge[M]
>[0];
export type BridgeResult<M extends BridgeMethod> = Awaited<
  ReturnType<DesktopBridge[M]>
>;
export const bridgeChannels = {
  bootstrap: "nv:bootstrap",
  openExternal: "nv:open-external",
  copy: "nv:copy",
  showWindow: "nv:show-window",
  releaseWindow: "nv:release-window",
  setLoginItem: "nv:login-item",
  getLoginItem: "nv:get-login-item",
  revealData: "nv:reveal-data",
  chooseProject: "nv:choose-project",
} as const satisfies Record<BridgeMethod, `nv:${string}`>;

declare global {
  interface Window {
    nonstopvibin?: DesktopBridge;
  }
}
