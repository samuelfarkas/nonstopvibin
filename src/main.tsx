import React from "react";
import { createRoot } from "react-dom/client";
import { App } from "./client/App.tsx";
import "@fontsource-variable/geist";
import "@fontsource-variable/geist-mono";
import "@fontsource-variable/space-grotesk";
import "./client/styles.css";
import "./client/focus.ts";

class ErrorBoundary extends React.Component<
  { children: React.ReactNode },
  { error: boolean }
> {
  state = { error: false };
  static getDerivedStateFromError() {
    return { error: true };
  }
  render() {
    return this.state.error ? (
      <div className="fatal">
        <h1>Something interrupted the interface.</h1>
        <p>Your profiles and credentials are still stored locally.</p>
        <button className="button primary" onClick={() => location.reload()}>
          Reload nonstopvibin
        </button>
      </div>
    ) : (
      this.props.children
    );
  }
}
createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <ErrorBoundary>
      <App />
    </ErrorBoundary>
  </React.StrictMode>,
);
