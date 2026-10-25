import { ConfirmDialog } from "../../components/Dialog";

/** Confirm the local reset without implying the provider's limit has changed. */
export function ClearCooldownDialog({
  name,
  open,
  pending,
  onConfirm,
  onCancel,
}: {
  name: string;
  open: boolean;
  pending: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  return (
    <ConfirmDialog
      open={open}
      title={`Clear proxy cooldown for ${name}?`}
      confirmLabel="Clear proxy cooldown"
      tone="primary"
      pending={pending}
      onConfirm={onConfirm}
      onCancel={onCancel}
    >
      <p>
        This clears the local proxy cooldown for {name}, allowing the next request to try it again.
        It does not reset the provider&apos;s quota or sign in again. A new 429 response can put it
        back into cooldown.
      </p>
    </ConfirmDialog>
  );
}
