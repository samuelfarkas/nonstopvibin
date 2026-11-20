import React from "react";
import { createRoot } from "react-dom/client";
import { openedWith } from "./shared/renderer-url.ts";
import "@fontsource-variable/geist";
import "@fontsource-variable/geist-mono";
import "@fontsource-variable/space-grotesk";
import "./client/styles.css";
import "./client/focus.ts";

// Shown when a root fails to load or render.
const fatal = (
  <div className="fatal">
    <h1>Something interrupted the interface.</h1>
    <p>Your profiles and credentials are still stored locally.</p>
    <button className="button primary" onClick={() => location.reload()}>
      Reload nonstopvibin
    </button>
  </div>
);

class ErrorBoundary extends React.Component<
  { children: React.ReactNode },
  { error: boolean }
> {
  state = { error: false };
  static getDerivedStateFromError() {
    return { error: true };
  }
  render() {
    return this.state.error ? fatal : this.props.children;
  }
}
const root = createRoot(document.getElementById("root")!);
// Each window loads only its own root and the modules and CSS it uses.
void (
  openedWith(location).tray
    ? import("./client/Tray.tsx").then((module) => module.TrayWindow)
    : import("./client/App.tsx").then((module) => module.App)
).then(
  (Root) =>
    root.render(
      <React.StrictMode>
        <ErrorBoundary>
          <Root />
        </ErrorBoundary>
      </React.StrictMode>,
    ),
  () => root.render(fatal),
);
