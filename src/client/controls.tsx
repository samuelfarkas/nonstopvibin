// Form controls of the main window. The tray window never loads them: it has
// no drafts to guard, and they carry the Radix dialog and select.
import type { ReactNode } from "react";
import * as Dialog from "@radix-ui/react-dialog";
import * as RadixSelect from "@radix-ui/react-select";
import { Check, ChevronDown, Copy, X } from "lucide-react";
import { useEffect, useState } from "react";
import { copy, desktop } from "./api.ts";
import { messageOf } from "./action.ts";

// The window helper only holds a deferred close after a guard vetoed an unload.
// The other half: `ask_to_close_main` and `releaseWindow` in crates/window/src/app.rs.
let closeVetoed = false;
export function useKeepDraft(needed: boolean) {
  useEffect(() => {
    const bridge = desktop;
    if (!needed || !bridge) return;
    const preventClose = (event: BeforeUnloadEvent) => {
      event.preventDefault();
      event.returnValue = true;
      closeVetoed = true;
    };
    window.addEventListener("beforeunload", preventClose);
    return () => {
      window.removeEventListener("beforeunload", preventClose);
      // Other guards installed in this commit still get to veto the close.
      queueMicrotask(() => {
        if (!closeVetoed) return;
        closeVetoed = false;
        void bridge.releaseWindow().catch(() => {
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
  onClose,
  wide = false,
}: {
  title: string;
  description?: string;
  children: ReactNode;
  onClose(): void;
  /** The dialog frame is Modal's; Add subscription's tabbed forms need the wider one. */
  wide?: boolean;
}) {
  // Callers mount a Modal only while it is open.
  useKeepDraft(true);
  // Capture before an autoFocus input mounts.
  const [opener] = useState(() => document.activeElement);
  return (
    <Dialog.Root
      open
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
  placeholder,
  disabled,
  title,
}: {
  value: T;
  onValueChange(value: T): void;
  /** `hint`, `title` and `placeholder` let a long value (a project path) show in full. */
  options: { value: T; label: ReactNode; hint?: string }[];
  id?: string;
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
  text: string | (() => Promise<string>);
  label?: string;
  onError?(error: string): void;
}) {
  const [copying, setCopying] = useState(false);
  const [copied, setCopied] = useState(false);
  return (
    <button
      className="button small"
      disabled={copying}
      onClick={async () => {
        try {
          if (text instanceof Function) {
            setCopying(true);
            await copy(await text());
          } else await copy(text);
          setCopied(true);
          setTimeout(() => setCopied(false), 1800);
        } catch (error) {
          onError?.(
            messageOf(
              error,
              "Clipboard unavailable. Select and copy the text.",
            ),
          );
        } finally {
          setCopying(false);
        }
      }}
    >
      {copied ? <Check size={14} /> : <Copy size={14} />}{" "}
      {copying ? "Copying…" : copied ? "Copied" : label}
    </button>
  );
}
