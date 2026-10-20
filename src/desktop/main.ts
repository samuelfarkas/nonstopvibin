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
  shell,
  Tray,
  type IpcMainInvokeEvent,
} from "electron";
import { join } from "node:path";
import { chmodSync, mkdirSync } from "node:fs";
import { spawn } from "node:child_process";
import { createInterface } from "node:readline";
import { z } from "zod";
import { Application } from "../server/server.ts";
import { linuxLoginItem, setLinuxLoginItem } from "./autostart.ts";
import { errorMessage } from "../server/errors.ts";
import { dataDirectory } from "../server/data-directory.ts";
import { updatesEnabled } from "../../package.json";
import { updateChecker } from "./updates.ts";

app.setName("NonstopVibin");
const userData = dataDirectory();
mkdirSync(userData, { recursive: true, mode: 0o700 });
chmodSync(userData, 0o700);
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
let trayLoading: Promise<void> | undefined;
let mainBounds: Electron.Rectangle | undefined;
const deferredCloses = new WeakSet<BrowserWindow>();
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
const externalHosts = new Set([
  "auth.openai.com",
  "chatgpt.com",
  "claude.ai",
  "console.anthropic.com",
  "platform.claude.com",
  "accounts.google.com",
  "auth.kimi.com",
  "www.kimi.com",
  "kimi.com",
  "accounts.x.ai",
  "auth.x.ai",
  "grok.com",
  "accounts.xai.com",
  "opencode.ai",
]);
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
  application.core.report(errorMessage(error));
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
  if (profileId) application.store.profile(profileId);
  const existing = mainWindow;
  if (!mainWindow) {
    mainWindow = createWindow("main");
    mainLoading = mainWindow.loadURL(
      profileId
        ? `${application.origin}/?profile=${encodeURIComponent(profileId)}`
        : application.origin,
    );
  }
  const window = mainWindow;
  try {
    await mainLoading;
    if (existing && profileId)
      await window.loadURL(
        `${application.origin}/?profile=${encodeURIComponent(profileId)}`,
      );
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
          icon: join(root, "dist/client/icon.png"),
        }
      : {
          title: "NonstopVibin quotas",
          width: 386,
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
  window.webContents.session.setPermissionRequestHandler(
    (_contents, _permission, callback) => callback(false),
  );
  window.webContents.setWindowOpenHandler(({ url }) => {
    try {
      void shell.openExternal(safeExternal(url)).catch(reportDesktopError);
    } catch (error) {
      application.core.report(errorMessage(error));
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
      // A form's beforeunload guard keeps its draft in memory until finished.
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
    if (trayWindow === window) {
      trayWindow = undefined;
      trayLoading = undefined;
    }
  });
  return window;
}
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
  const root = app.getAppPath();
  application = await Application.create({
    directory: userData,
    binary: app.isPackaged
      ? join(process.resourcesPath, "core/cli-proxy-api")
      : join(root, ".vendor/core/cli-proxy-api"),
    clientDirectory: join(root, "dist/client"),
    port: Number(process.env.NONSTOPVIBIN_PORT || 4318),
    desktop: true,
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
  ipcMain.handle("nv:bootstrap", (event) => {
    assertSender(event);
    return { token: application.token, desktop: true };
  });
  // Every privileged handler with a payload validates its sender, then its argument.
  function handle<T, R>(
    channel: string,
    schema: z.ZodType<T>,
    run: (value: T) => R,
  ): void {
    ipcMain.handle(channel, (event, argument) => {
      assertSender(event);
      return run(schema.parse(argument));
    });
  }
  handle("nv:open-external", z.string().max(20_000), (url) =>
    shell.openExternal(safeExternal(url)),
  );
  handle("nv:copy", z.string().max(1_000_000), (text) =>
    clipboard.writeText(text),
  );
  handle("nv:show-window", z.string().uuid().optional(), showMain);
  ipcMain.handle("nv:release-window", (event, argument) => {
    assertSender(event);
    z.undefined().parse(argument);
    if (
      mainWindow &&
      event.sender === mainWindow.webContents &&
      deferredCloses.has(mainWindow) &&
      !mainWindow.isVisible()
    )
      mainWindow.close();
  });
  handle("nv:login-item", z.boolean(), (value) => {
    if (!app.isPackaged)
      throw new Error("Use the packaged app to enable automatic startup.");
    if (process.platform === "linux")
      return setLinuxLoginItem(
        app.getPath("appData"),
        process.env.APPIMAGE || process.execPath,
        value,
      );
    app.setLoginItemSettings({ openAtLogin: value, args: ["--background"] });
    const actual = app.getLoginItemSettings().openAtLogin;
    if (actual !== value)
      throw new Error(
        "macOS did not apply this setting. Add NonstopVibin in System Settings → General → Login Items.",
      );
    return actual;
  });
  ipcMain.handle("nv:get-login-item", (event) => {
    assertSender(event);
    return process.platform === "linux"
      ? linuxLoginItem(app.getPath("appData"))
      : app.getLoginItemSettings().openAtLogin;
  });
  ipcMain.handle("nv:reveal-data", (event) => {
    assertSender(event);
    shell.showItemInFolder(join(userData, "nonstopvibin.sqlite"));
  });
  ipcMain.handle("nv:choose-project", async (event) => {
    assertSender(event);
    if (!mainWindow) throw new Error("Open the main window first.");
    const result = await dialog.showOpenDialog(mainWindow, {
      title: "Connect Claude Code in a project",
      properties: ["openDirectory"],
      buttonLabel: "Choose project",
    });
    return result.canceled ? null : (result.filePaths[0] ?? null);
  });
  // Mirrorball status item (docs/design/icons.md): still image, colored while
  // a proxy runs, grey when none does. Full color on every platform.
  const trayImage = (name: string) =>
    nativeImage.createFromPath(
      join(
        root,
        `dist/client/tray/${process.platform === "darwin" ? "mac" : "linux"}-${name}.png`,
      ),
    );
  const ballImage = trayImage("ball");
  const stoppedImage = trayImage("stopped");
  tray = new Tray(stoppedImage);
  tray.setToolTip("NonstopVibin · proxy off");
  let trayState = "";
  let trayTooltip = "";
  const syncTray = () => {
    if (quitting) return;
    const running = application.store
      .profiles()
      .filter((p) => application.core.runtimes.get(p.id)?.state === "running");
    const streaming = running.some(
      (p) => (application.gateway.active.get(p.id) ?? 0) > 0,
    );
    const state = running.length ? "running" : "stopped";
    const names = running.map((p) => p.name).join(", ");
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
  application.core.onStatusChange = syncTray;
  application.core.onOAuthCallback = () => {
    // Only an existing window holds the sign-in dialog; a new one would not.
    if (mainWindow) void showMain().catch(reportDesktopError);
  };
  application.gateway.onActivityChange = syncTray;
  syncTray();
  const openTray = async () => {
    if (quitting) return;
    if (trayWindow?.isVisible()) {
      trayWindow.close();
      return;
    }
    if (trayWindow) return;
    trayWindow = createWindow("tray");
    trayLoading = trayWindow.loadURL(`${application.origin}/?view=tray`);
    const window = trayWindow;
    await trayLoading;
    if (window.isDestroyed() || quitting) return;
    if (process.platform === "darwin") await watchOutsideClicks(window);
    if (window.isDestroyed() || quitting) return;
    const bounds = tray.getBounds();
    const display = screen.getDisplayMatching(bounds).workArea;
    const height = Math.min(640, display.height - 24);
    const x = Math.max(
      display.x + 8,
      Math.min(
        bounds.x + bounds.width / 2 - 193,
        display.x + display.width - 394,
      ),
    );
    window.setBounds({
      x: Math.round(x),
      y: Math.round(
        process.platform === "darwin"
          ? bounds.y + bounds.height + 7
          : display.y + display.height - height - 8,
      ),
      width: 386,
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
