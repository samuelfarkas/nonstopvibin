import type { ReactNode } from "react";
import * as Dialog from "@radix-ui/react-dialog";
import * as RadixSelect from "@radix-ui/react-select";
import { Activity, Bot, Check, ChevronDown, Copy, X } from "lucide-react";
import { useEffect, useState } from "react";
import type { Account, QuotaWindow } from "../shared/types.ts";
import { copy } from "./api.ts";
import { accountLabel, exactTime, resetIn } from "./format.ts";

export function useKeepDraft(needed: boolean) {
  useEffect(() => {
    if (!needed || !window.nonstopvibin) return;
    const preventClose = (event: BeforeUnloadEvent) => {
      event.preventDefault();
      event.returnValue = true;
    };
    window.addEventListener("beforeunload", preventClose);
    return () => {
      window.removeEventListener("beforeunload", preventClose);
      // Other guards installed in this commit still get to veto the close.
      queueMicrotask(() => {
        void window.nonstopvibin?.releaseWindow().catch(() => {
          // The renderer can disappear before the IPC reply during normal unload.
        });
      });
    };
  }, [needed]);
}

export function Modal({
  title,
  description,
  children,
  open,
  onClose,
  wide = false,
}: {
  title: string;
  description?: string;
  children: ReactNode;
  open: boolean;
  onClose(): void;
  wide?: boolean;
}) {
  useKeepDraft(open);
  // Capture before an autoFocus input mounts; callers mount dialogs when opened.
  const [opener] = useState(() => document.activeElement);
  return (
    <Dialog.Root
      open={open}
      onOpenChange={(value) => {
        if (!value) onClose();
      }}
    >
      <Dialog.Portal>
        <Dialog.Overlay className="modal-overlay" />
        <Dialog.Content
          className={`modal ${wide ? "wide" : ""}`}
          onCloseAutoFocus={(event) => {
            if (opener instanceof HTMLElement && opener.isConnected) {
              event.preventDefault();
              opener.focus();
            }
          }}
        >
          <div className="modal-heading">
            <div>
              <Dialog.Title>{title}</Dialog.Title>
              <Dialog.Description>{description || ""}</Dialog.Description>
            </div>
            <Dialog.Close className="icon-button" aria-label="Close dialog">
              <X size={19} />
            </Dialog.Close>
          </div>
          {children}
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
export function Select<T extends string>({
  value,
  onValueChange,
  options,
  id,
  name,
  placeholder,
  disabled,
  title,
}: {
  value: T;
  onValueChange(value: T): void;
  options: { value: T; label: ReactNode; hint?: string }[];
  id?: string;
  name?: string;
  placeholder?: string;
  disabled?: boolean;
  title?: string;
}) {
  return (
    <RadixSelect.Root
      value={value}
      onValueChange={(next) =>
        onValueChange(options.find((o) => o.value === next)?.value ?? value)
      }
      disabled={disabled}
      name={name}
    >
      <RadixSelect.Trigger className="select" id={id} title={title}>
        <RadixSelect.Value placeholder={placeholder} />
        <RadixSelect.Icon className="select-chevron">
          <ChevronDown size={14} />
        </RadixSelect.Icon>
      </RadixSelect.Trigger>
      <RadixSelect.Portal>
        <RadixSelect.Content
          className="select-menu"
          position="popper"
          sideOffset={4}
        >
          <RadixSelect.Viewport>
            {options.map((option) => (
              <RadixSelect.Item
                key={option.value}
                value={option.value}
                className="select-item"
              >
                <RadixSelect.ItemText>{option.label}</RadixSelect.ItemText>
                {option.hint && (
                  <span className="select-hint">{option.hint}</span>
                )}
                <RadixSelect.ItemIndicator className="select-check">
                  <Check size={14} />
                </RadixSelect.ItemIndicator>
              </RadixSelect.Item>
            ))}
          </RadixSelect.Viewport>
        </RadixSelect.Content>
      </RadixSelect.Portal>
    </RadixSelect.Root>
  );
}
export function CopyButton({
  text,
  label = "Copy",
  onError,
}: {
  text: string;
  label?: string;
  onError?(error: string): void;
}) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      className="button small"
      onClick={async () => {
        try {
          await copy(text);
          setCopied(true);
          setTimeout(() => setCopied(false), 1800);
        } catch (error) {
          onError?.(
            error instanceof Error
              ? error.message
              : "Clipboard unavailable. Select and copy the text.",
          );
        }
      }}
    >
      {copied ? <Check size={14} /> : <Copy size={14} />}{" "}
      {copied ? "Copied" : label}
    </button>
  );
}
export function AnimatedLogo({
  state,
}: {
  state: "running" | "paused" | "stopped";
}) {
  const [replay, setReplay] = useState(0);
  return (
    <button
      type="button"
      className="nv-logo-button"
      aria-label="NonstopVibin — replay logo animation"
      onClick={() => setReplay((value) => value + 1)}
    >
      <Logo key={replay} state={state} />
    </button>
  );
}

export function Logo({
  state = "stopped",
  size = 24,
  word = true,
}: {
  state?: "running" | "paused" | "stopped";
  size?: number;
  word?: boolean;
}) {
  return (
    <span className="nv-logo">
      <span className="nv-ball-wrap" aria-hidden="true">
        <img
          className="nv-ball"
          data-state={state}
          src={`/tray/${state === "stopped" ? "stopped" : "ball"}.svg`}
          width={size}
          height={size}
          alt=""
        />
      </span>
      {word && (
        <span className="nv-text">
          <span className="nv-word">NONSTOP</span>
          <span className="nv-sub">VIBIN</span>
        </span>
      )}
    </span>
  );
}
const providerAssets = new Map([
  ["claude", "claude"],
  ["anthropic", "claude"],
  ["codex", "openai"],
  ["openai", "openai"],
  ["kimi", "kimi"],
  ["antigravity", "antigravity"],
  ["xai", "xai"],
  ["opencode-go", "opencode"],
]);
export function ProviderIcon({ provider }: { provider: string }) {
  const asset = providerAssets.get(provider);
  return (
    <span className={`provider-icon ${provider}`}>
      {asset ? (
        <img alt="" src={`/providers/${asset}.svg`} />
      ) : (
        <Bot size={25} />
      )}
    </span>
  );
}
export function Status({ value }: { value: string }) {
  return (
    <span
      className={`status ${value === "running" || value === "Ready" ? "good" : value === "error" || value === "Needs attention" || value === "Reconnect needed" ? "bad" : ""}`}
    >
      <span className="status-dot" />
      {value === "running"
        ? "Running"
        : value === "stopped"
          ? "Stopped"
          : value === "starting"
            ? "Starting…"
            : value}
    </span>
  );
}
export const meterLevel = (pct: number | null | undefined) =>
  pct === null || pct === undefined
    ? "unknown"
    : pct <= 0
      ? "empty"
      : pct < 20
        ? "low"
        : "";
export function QuotaMeter({
  window,
  stale = false,
  unavailable = "not reported",
  label,
}: {
  window?: QuotaWindow;
  stale?: boolean;
  unavailable?: string;
  label?: string;
}) {
  const pct = window?.remainingPercent;
  const level = meterLevel(pct);
  return (
    <div className={`quota-meter ${stale ? "stale" : ""} ${level}`}>
      <span className="label" title={label ?? window?.label}>
        {label ?? window?.label ?? "limit"}
        {window?.scoped && <small> model only</small>}
      </span>
      <div className={`quota-value ${level}`}>
        {pct === null || pct === undefined ? (
          <>
            — <small>{unavailable}</small>
          </>
        ) : (
          `${Math.round(pct)}%`
        )}
      </div>
      <div
        className={`meter-track ${level}`}
        role={pct == null ? undefined : "meter"}
        aria-hidden={pct == null || undefined}
        aria-label={label ?? window?.label}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={pct ?? undefined}
      >
        <div style={{ width: `${pct ?? 0}%` }} />
      </div>
      <div className="reset" title={exactTime(window?.resetsAt ?? null)}>
        {!window
          ? "—"
          : window.resetsAt || pct !== 100
            ? resetIn(window.resetsAt)
            : "nothing used yet"}
      </div>
    </div>
  );
}
export function AccountSummary({ account }: { account: Account }) {
  return (
    <div className="account-identity">
      <ProviderIcon provider={account.provider} />
      <div>
        <strong title={accountLabel(account)}>{accountLabel(account)}</strong>
        <div className="account-email">
          {account.organizationUuid && "Claude · "}
          {account.email ||
            (account.kind === "api-key"
              ? `${account.prefix ? `${account.prefix}/ · ` : ""}${account.modelCount} model${account.modelCount === 1 ? "" : "s"}`
              : "OAuth subscription")}
        </div>
        {account.provider === "claude" &&
          account.kind === "oauth" &&
          !account.organizationUuid && (
            <small className="identity-unknown">Organization unverified</small>
          )}
      </div>
    </div>
  );
}
export function Empty({
  icon,
  title,
  children,
  action,
}: {
  icon?: ReactNode;
  title: string;
  children: ReactNode;
  action?: ReactNode;
}) {
  return (
    <div className="empty-state">
      <div className="empty-icon">{icon || <Activity size={22} />}</div>
      <h2>{title}</h2>
      <p>{children}</p>
      {action}
    </div>
  );
}
