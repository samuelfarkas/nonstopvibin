import { contextBridge, ipcRenderer } from "electron";
import {
  bridgeChannels,
  type BridgeArgument,
  type BridgeMethod,
} from "../shared/desktop-bridge.ts";

// Main validates each argument; the preload only forwards it.
contextBridge.exposeInMainWorld(
  "nonstopvibin",
  Object.fromEntries(
    Object.entries(bridgeChannels).map(([method, channel]) => [
      method,
      (argument?: BridgeArgument<BridgeMethod>) =>
        ipcRenderer.invoke(channel, argument),
    ]),
  ),
);
