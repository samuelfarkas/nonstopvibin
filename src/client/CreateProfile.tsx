import { useState } from "react";
import { ArrowRight, ShieldCheck } from "lucide-react";
import { nameLength } from "../shared/vocabulary.ts";
import { api } from "./api.ts";
import { useAction } from "./action.ts";
import { Modal } from "./controls.tsx";
import "./create-profile.css";

export function CreateProfile({
  onClose,
  onCreated,
}: {
  onClose(): void;
  onCreated(id: string): Promise<void>;
}) {
  const [name, setName] = useState("");
  const { busy, error, run } = useAction();
  return (
    <Modal
      title="Create a profile"
      description="A separate group of subscriptions with its own API key."
      onClose={onClose}
    >
      <form
        onSubmit={(e) => {
          e.preventDefault();
          void run(async () => {
            const p = await api("POST /profiles", { body: { name } });
            await onCreated(p.id);
          }, "Could not create profile.");
        }}
      >
        {error && (
          <div className="inline-error" role="alert">
            {error}
          </div>
        )}
        <label className="field">
          Profile name
          <input
            autoFocus
            required
            maxLength={nameLength}
            placeholder="Personal, Company A, Side projects…"
            value={name}
            onChange={(e) => setName(e.target.value)}
          />
        </label>
        <div className="info-note">
          <ShieldCheck size={18} />
          <p>
            Subscriptions in this profile stay together. Requests never fall
            back to another profile.
          </p>
        </div>
        <div className="modal-footer">
          <button type="button" className="button" onClick={onClose}>
            Cancel
          </button>
          <button className="button primary" disabled={busy || !name.trim()}>
            {busy ? "Creating…" : "Create profile"}
            <ArrowRight size={15} />
          </button>
        </div>
      </form>
    </Modal>
  );
}
