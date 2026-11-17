import {
  app,
  BrowserWindow,
  clipboard,
  dialog,
  ipcMain,
  Menu,
  nativeImage,
  nativeTheme,
  screen,
  session,
  shell,
  Tray,
  type IpcMainInvokeEvent,
} from "electron";
import { join } from "node:path";
import { mkdirSync } from "node:fs";
import { spawn } from "node:child_process";
import { createInterface } from "node:readline";
import { z } from "zod";
import { Application } from "../server/server.ts";
import { linuxLoginItem, setLinuxLoginItem } from "./autostart.ts";
import { AppError, errorMessage } from "../server/errors.ts";
import {
  dataDirectory,
  databasePath,
  ownerOnlyDirectory,
} from "../server/data-directory.ts";
import { providers, type ProviderEntry } from "../shared/providers.ts";
import {
  bridgeChannels,
  type BridgeArgument,
  type BridgeMethod,
  type BridgeResult,
} from "../shared/desktop-bridge.ts";
import { mainUrl, trayUrl } from "../shared/renderer-url.ts";
import { updatesEnabled } from "../../package.json";
import { updateChecker } from "./updates.ts";
import {
  checkoutLayout,
  hostPort,
  terminationSignals,
} from "../server/host-defaults.ts";

app.setName("NonstopVibin");
const userData = dataDirectory();
ownerOnlyDirectory(userData);
// Set every Electron storage path before readiness or the single-instance lock.
app.setPath("userData", userData);
for (const name of ["sessionData", "crashDumps"] as const) {
  const directory = join(userData, name);
  mkdirSync(directory, { recursive: true, mode: 0o700 });
  app.setPath(name, directory);
}
app.setAppLogsPath(join(userData, "logs"));
let application: Application;
let mainWindow: BrowserWindow | undefined;
let trayWindow: BrowserWindow | undefined;
let mainLoading: Promise<void> | undefined;
let mainBounds: Electron.Rectangle | undefined;
const deferredCloses = new WeakSet<BrowserWindow>();
/** The menu-bar panel; its CSS fills whatever width the window has. */
const trayWidth = 386;
let ready = false;
let openOnReady = !process.argv.includes("--background");
let tray: Tray;
let quitting = false;
const single = app.requestSingleInstanceLock();
if (!single) app.quit();
else {
  app.on("second-instance", () => {
    if (!ready) openOnReady = true;
    else void showMain().catch(reportDesktopError);
  });
  app.on("activate", () => {
    if (ready && !trayWindow) void showMain().catch(reportDesktopError);
  });
  app.on("window-all-closed", () => {
    /* The menu bar and proxy intentionally continue until Quit. */
  });
  // Route termination signals through the orderly quit, which stops every core.
  for (const signal of terminationSignals) process.on(signal, () => app.quit());
  app.on("before-quit", (event) => {
    if (quitting) return;
    event.preventDefault();
    quitting = true;
    if (application)
      application.close().then(
        () => app.exit(0),
        (error) => {
          dialog.showErrorBox("Could not shut down cleanly", String(error));
          app.exit(1);
        },
      );
    else app.exit(0);
  });
  void app
    .whenReady()
    .then(start)
    .catch((error) => {
      const message = error instanceof Error ? error.message : String(error);
      console.error(`NonstopVibin could not start: ${message}`);
      dialog.showErrorBox("NonstopVibin could not start", message);
      app.quit();
    });
}
// Each provider's sign-in and account pages.
const externalHosts = new Set(
  Object.values<ProviderEntry>(providers).flatMap(
    (provider) => provider.hosts ?? [],
  ),
);
function safeExternal(raw: string): string {
  const url = new URL(raw);
  if (
    url.protocol !== "https:" ||
    url.username ||
    url.password ||
    !externalHosts.has(url.hostname)
  )
    throw new Error(
      "This external URL is not an approved provider sign-in page.",
    );
  return url.href;
}
function reportDesktopError(error: Error) {
  application.report(errorMessage(error));
}
async function watchOutsideClicks(window: BrowserWindow): Promise<void> {
  const root = app.getAppPath();
  const monitor = spawn(
    join(
      app.isPackaged ? `${root}.unpacked` : root,
      "dist/desktop/tray-click-monitor",
    ),
    [],
    { stdio: ["pipe", "pipe", "ignore"] },
  );
  const lines = createInterface({ input: monitor.stdout });
  window.once("closed", () => {
    lines.close();
    monitor.stdin.end();
    monitor.kill();
  });
  await new Promise<void>((resolve, reject) => {
    const timeout = setTimeout(
      () => fail(new Error("The menu bar click monitor did not start.")),
      5000,
    );
    const fail = (error: Error) => {
      clearTimeout(timeout);
      if (!window.isDestroyed()) window.close();
      reject(error);
    };
    monitor.once("error", fail);
    monitor.once("exit", () => {
      fail(new Error("The menu bar click monitor stopped."));
    });
    lines.on("line", (line) => {
      if (line === "ready") {
        clearTimeout(timeout);
        return resolve();
      }
      if (window.isDestroyed() || !window.isVisible()) return;
      const point = line.split(" ").map(Number);
      if (point.length !== 2 || !point.every(Number.isFinite)) return;
      const [x, y] = point;
      if (x === undefined || y === undefined) return;
      const inside = (bounds: Electron.Rectangle) =>
        x >= bounds.x &&
        x < bounds.x + bounds.width &&
        y >= bounds.y &&
        y < bounds.y + bounds.height;
      if (!inside(window.getBounds()) && !inside(tray.getBounds()))
        window.close();
    });
  });
}
async function showMain(profileId?: string) {
  if (quitting) return;
  if (profileId && !application.hasProfile(profileId))
    throw new AppError("Profile not found.", 404);
  const existing = mainWindow;
  if (!mainWindow) {
    mainWindow = createWindow("main");
    mainLoading = mainWindow.loadURL(mainUrl(application.origin, profileId));
  }
  const window = mainWindow;
  try {
    await mainLoading;
    if (existing && profileId)
      await window.loadURL(mainUrl(application.origin, profileId));
  } finally {
    if (!quitting && !window.isDestroyed()) {
      trayWindow?.close();
      window.show();
      window.focus();
    }
  }
}
function createWindow(view: "main" | "tray"): BrowserWindow {
  const root = app.getAppPath();
  const window = new BrowserWindow({
    ...(view === "main"
      ? {
          title: "NonstopVibin",
          width: 1240,
          height: 880,
          minWidth: 740,
          minHeight: 580,
          ...mainBounds,
          titleBarStyle:
            process.platform === "darwin" ? "hiddenInset" : "default",
          trafficLightPosition: { x: 21, y: 17 },
          icon: join(root, checkoutLayout.client, "icon.png"),
        }
      : {
          title: "NonstopVibin quotas",
          width: trayWidth,
          height: 640,
          frame: false,
          resizable: false,
          skipTaskbar: true,
          alwaysOnTop: true,
        }),
    show: false,
    paintWhenInitiallyHidden: false,
    backgroundColor: "#1c1b18",
    webPreferences: {
      preload: join(__dirname, "preload.cjs"),
      sandbox: true,
      contextIsolation: true,
      nodeIntegration: false,
      devTools: !app.isPackaged,
    },
  });
  window.webContents.setWindowOpenHandler(({ url }) => {
    try {
      void shell.openExternal(safeExternal(url)).catch(reportDesktopError);
    } catch (error) {
      application.report(errorMessage(error));
    }
    return { action: "deny" };
  });
  window.webContents.on("will-navigate", (event, url) => {
    if (!URL.canParse(url) || new URL(url).origin !== application.origin)
      event.preventDefault();
  });
  if (view === "main") {
    let closing = false;
    window.on("close", () => {
      mainBounds = window.getNormalBounds();
      closing = true;
    });
    window.webContents.on("will-prevent-unload", (event) => {
      if (quitting) event.preventDefault();
      // A form's beforeunload guard keeps its draft in memory until finished;
      // useKeepDraft (client/controls.tsx) vetoes, then calls releaseWindow.
      if (closing && !quitting) {
        deferredCloses.add(window);
        window.hide();
      }
      closing = false;
    });
    window.on("show", () => deferredCloses.delete(window));
  } else {
    if (process.platform !== "darwin") window.on("blur", () => window.close());
    window.webContents.on("before-input-event", (event, input) => {
      if (input.type === "keyDown" && input.key === "Escape") {
        event.preventDefault();
        window.close();
      }
    });
  }
  window.on("closed", () => {
    if (mainWindow === window) {
      mainWindow = undefined;
      mainLoading = undefined;
    }
    if (trayWindow === window) trayWindow = undefined;
  });
  return window;
}
interface BridgeHandler<M extends BridgeMethod> {
  argument: z.ZodType<BridgeArgument<M>>;
  run(
    argument: BridgeArgument<M>,
    event: IpcMainInvokeEvent,
  ): BridgeResult<M> | Promise<BridgeResult<M>>;
}
type BridgeHandlers = { [M in BridgeMethod]: BridgeHandler<M> };
const isBridgeMethod = (method: string): method is BridgeMethod =>
  Object.hasOwn(bridgeChannels, method);
function assertSender(event: IpcMainInvokeEvent): void {
  const sender = event.senderFrame;
  if (
    !sender ||
    sender !== event.sender.mainFrame ||
    new URL(sender.url).origin !== application.origin ||
    ![mainWindow, trayWindow].some(
      (window) =>
        window && !window.isDestroyed() && window.webContents === event.sender,
    )
  )
    throw new Error("Untrusted desktop request.");
}
async function start(): Promise<void> {
  // Every window uses the default session; deny permissions before any can exist.
  session.defaultSession.setPermissionRequestHandler(
    (_contents, _permission, callback) => callback(false),
  );
  const root = app.getAppPath();
  application = await Application.create({
    directory: userData,
    binary: app.isPackaged
      ? join(process.resourcesPath, "core/cli-proxy-api")
      : join(root, checkoutLayout.core),
    clientDirectory: join(root, checkoutLayout.client),
    port: hostPort(),
    desktop: true,
    // Without its core the window still opens; each profile start reports it missing.
    allowMissingCore: true,
  });
  // The UI is dark-only; force the app appearance so native popups (select
  // menus, dialogs) match instead of following a light OS theme.
  nativeTheme.themeSource = "dark";
  const updateMenu: Electron.MenuItemConstructorOptions = {
    label: "Check for updates…",
    enabled: false,
  };
  if (
    updatesEnabled &&
    app.isPackaged &&
    (process.platform === "darwin" ||
      (process.platform === "linux" && process.env.APPIMAGE))
  ) {
    // AppImage updates may rename the executable; repair an existing login entry on launch.
    if (
      process.platform === "linux" &&
      process.env.APPIMAGE &&
      linuxLoginItem(app.getPath("appData"))
    )
      await setLinuxLoginItem(
        app.getPath("appData"),
        process.env.APPIMAGE,
        true,
      ).catch(reportDesktopError);
    const { autoUpdater } = await import("electron-updater");
    autoUpdater.autoDownload = false;
    autoUpdater.autoInstallOnAppQuit = false;
    autoUpdater.allowPrerelease = app.getVersion().includes("-");
    autoUpdater.allowDowngrade = false;
    autoUpdater.logger = null;
    let installing = false;
    autoUpdater.on("error", () => {
      console.warn("App updater reported an error.");
      if (installing) {
        dialog.showErrorBox(
          "Could not install update",
          "Please reopen NonstopVibin and try again.",
        );
        app.exit(1);
      }
    });
    const check = updateChecker(
      autoUpdater,
      dialog,
      async (install) => {
        if (quitting) return;
        quitting = true;
        try {
          await application.close();
          installing = true;
          install();
        } catch {
          dialog.showErrorBox(
            "Could not restart for update",
            "Please reopen NonstopVibin and try again.",
          );
          app.exit(1);
        }
      },
      () => quitting,
    );
    updateMenu.enabled = true;
    updateMenu.click = () => {
      void check(true);
    };
    const startup = setTimeout(() => {
      void check();
    }, 30_000);
    const interval = setInterval(
      () => {
        void check();
      },
      6 * 60 * 60 * 1000,
    );
    startup.unref();
    interval.unref();
    app.once("before-quit", () => {
      clearTimeout(startup);
      clearInterval(interval);
    });
  }
  const none = z.undefined();
  const handlers: BridgeHandlers = {
    bootstrap: { argument: none, run: () => ({ token: application.token }) },
    openExternal: {
      argument: z.string().max(20_000),
      run: (url) => shell.openExternal(safeExternal(url)),
    },
    copy: {
      argument: z.string().max(1_000_000),
      run: (text) => clipboard.writeText(text),
    },
    showWindow: {
      argument: z.string().uuid().optional(),
      run: (profileId) => showMain(profileId),
    },
    // Closes a main window that useKeepDraft (client/controls.tsx) kept open.
    releaseWindow: {
      argument: none,
      run: (_, event) => {
        if (
          mainWindow &&
          event.sender === mainWindow.webContents &&
          deferredCloses.has(mainWindow) &&
          !mainWindow.isVisible()
        )
          mainWindow.close();
      },
    },
    setLoginItem: {
      argument: z.boolean(),
      run: (value) => {
        if (!app.isPackaged)
          throw new Error("Use the packaged app to enable automatic startup.");
        if (process.platform === "linux")
          return setLinuxLoginItem(
            app.getPath("appData"),
            process.env.APPIMAGE || process.execPath,
            value,
          );
        app.setLoginItemSettings({
          openAtLogin: value,
          args: ["--background"],
        });
        const actual = app.getLoginItemSettings().openAtLogin;
        if (actual !== value)
          throw new Error(
            "macOS did not apply this setting. Add NonstopVibin in System Settings → General → Login Items.",
          );
        return actual;
      },
    },
    getLoginItem: {
      argument: none,
      run: () =>
        process.platform === "linux"
          ? linuxLoginItem(app.getPath("appData"))
          : app.getLoginItemSettings().openAtLogin,
    },
    revealData: {
      argument: none,
      run: () => shell.showItemInFolder(databasePath(userData)),
    },
    chooseProject: {
      argument: none,
      run: async () => {
        if (!mainWindow) throw new Error("Open the main window first.");
        const result = await dialog.showOpenDialog(mainWindow, {
          title: "Connect Claude Code in a project",
          properties: ["openDirectory"],
          buttonLabel: "Choose project",
        });
        return result.canceled ? null : (result.filePaths[0] ?? null);
      },
    },
  };
  // Every privileged handler validates its sender, then its argument.
  function handle<M extends BridgeMethod>(method: M): void {
    const { argument, run } = handlers[method];
    ipcMain.handle(bridgeChannels[method], (event, value) => {
      assertSender(event);
      return run(argument.parse(value), event);
    });
  }
  for (const method of Object.keys(bridgeChannels).filter(isBridgeMethod))
    handle(method);
  // Mirrorball status item (docs/design/icons.md): still image, colored while
  // a proxy runs, grey when none does. Full color on every platform.
  const trayImage = (name: string) =>
    nativeImage.createFromPath(
      join(
        root,
        checkoutLayout.client,
        `tray/${process.platform === "darwin" ? "mac" : "linux"}-${name}.png`,
      ),
    );
  const ballImage = trayImage("ball");
  const stoppedImage = trayImage("stopped");
  tray = new Tray(stoppedImage);
  tray.setToolTip("NonstopVibin · proxy off");
  let trayState = "";
  let trayTooltip = "";
  // Gateway idle/active edges run this per request.
  const syncTray = () => {
    if (quitting) return;
    const { running, streaming } = application.status();
    const state = running.length ? "running" : "stopped";
    const names = running.join(", ");
    const tooltip =
      state === "stopped"
        ? "NonstopVibin · proxy off"
        : `NonstopVibin — ${names} · ${streaming ? "streaming" : "idle"}`;
    if (tooltip !== trayTooltip) {
      tray.setToolTip(tooltip);
      trayTooltip = tooltip;
    }
    if (state === trayState) return;
    trayState = state;
    tray.setImage(state === "stopped" ? stoppedImage : ballImage);
  };
  application.on("status", syncTray);
  application.on("sign-in", () => {
    // Only an existing window holds the sign-in dialog; a new one would not.
    if (mainWindow) void showMain().catch(reportDesktopError);
  });
  syncTray();
  const openTray = async () => {
    if (quitting) return;
    if (trayWindow?.isVisible()) {
      trayWindow.close();
      return;
    }
    if (trayWindow) return;
    trayWindow = createWindow("tray");
    const window = trayWindow;
    try {
      await window.loadURL(trayUrl(application.origin));
    } catch (error) {
      // Destroying emits "closed", which clears trayWindow so a later click can retry.
      if (!window.isDestroyed()) window.destroy();
      throw error;
    }
    if (window.isDestroyed() || quitting) return;
    if (process.platform === "darwin") await watchOutsideClicks(window);
    if (window.isDestroyed() || quitting) return;
    const bounds = tray.getBounds();
    const display = screen.getDisplayMatching(bounds).workArea;
    const height = Math.min(640, display.height - 24);
    const x = Math.max(
      display.x + 8,
      Math.min(
        bounds.x + bounds.width / 2 - trayWidth / 2,
        display.x + display.width - trayWidth - 8,
      ),
    );
    window.setBounds({
      x: Math.round(x),
      y: Math.round(
        process.platform === "darwin"
          ? bounds.y + bounds.height + 7
          : display.y + display.height - height - 8,
      ),
      width: trayWidth,
      height,
    });
    window.show();
    window.focus();
  };
  const toggleTray = () => {
    void openTray().catch(reportDesktopError);
  };
  tray.on("click", toggleTray);
  tray.on("right-click", () =>
    tray.popUpContextMenu(
      Menu.buildFromTemplate([
        {
          label: "Open NonstopVibin",
          click: () => {
            void showMain().catch(reportDesktopError);
          },
        },
        { label: "Subscription quotas", click: toggleTray },
        updateMenu,
        { type: "separator" },
        { label: "Quit NonstopVibin", click: () => app.quit() },
      ]),
    ),
  );
  Menu.setApplicationMenu(
    Menu.buildFromTemplate([
      {
        label: "NonstopVibin",
        submenu: [
          { role: "about" },
          updateMenu,
          { type: "separator" },
          { role: "hide" },
          { role: "hideOthers" },
          { role: "unhide" },
          { type: "separator" },
          { role: "quit" },
        ],
      },
      { role: "editMenu" },
      {
        label: "View",
        submenu: [
          {
            label: "Subscription quotas",
            accelerator: "CmdOrCtrl+Shift+U",
            click: toggleTray,
          },
          {
            label: "Open main window",
            accelerator: "CmdOrCtrl+1",
            click: () => {
              void showMain().catch(reportDesktopError);
            },
          },
          { type: "separator" },
          { role: "resetZoom" },
          { role: "zoomIn" },
          { role: "zoomOut" },
          { role: "togglefullscreen" },
        ],
      },
      { role: "windowMenu" },
    ]),
  );
  ready = true;
  const loginLaunch =
    process.platform === "darwin" &&
    app.getLoginItemSettings().wasOpenedAtLogin;
  if (openOnReady && !loginLaunch) await showMain();
}
