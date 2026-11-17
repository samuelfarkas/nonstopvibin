import { useState } from "react";

/** The text to show for a failure; `fallback` when it carries none. */
export function messageOf(cause: unknown, fallback: string): string {
  return cause instanceof Error ? cause.message : fallback;
}

/** One user action at a time, with the failure it left. */
export function useAction() {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  async function run(action: () => Promise<void>, fallback: string) {
    setBusy(true);
    setError("");
    try {
      await action();
    } catch (cause) {
      setError(messageOf(cause, fallback));
    } finally {
      setBusy(false);
    }
  }
  return { busy, error, setError, run };
}
