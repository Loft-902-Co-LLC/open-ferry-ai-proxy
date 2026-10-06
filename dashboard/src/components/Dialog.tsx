import { useEffect, useId, useRef, type ReactNode } from "react";

import { Button } from "./Button";
import { Spinner } from "./Spinner";

export interface DialogProps {
  open: boolean;
  title: ReactNode;
  children?: ReactNode;
  /** Called when the dialog closes by Escape or a close button. */
  onClose: () => void;
  /** The buttons at the bottom. */
  footer?: ReactNode;
}

/**
 * A modal dialog, on the native <dialog> element: the browser traps focus,
 * closes it with Escape and makes the page behind it inert, with no
 * injected styles or scroll lock. Mark the control to focus first with
 * `data-autofocus`.
 */
export function Dialog({ open, title, children, onClose, footer }: DialogProps) {
  const ref = useRef<HTMLDialogElement>(null);
  const titleId = useId();

  useEffect(() => {
    const dialog = ref.current;
    if (dialog === null) {
      return;
    }
    if (open && !dialog.open) {
      dialog.showModal();
      // The browser focuses the first control; a dialog may name another.
      dialog.querySelector<HTMLElement>("[data-autofocus]")?.focus();
    } else if (!open && dialog.open) {
      dialog.close();
    }
  }, [open]);

  return (
    <dialog
      ref={ref}
      aria-labelledby={titleId}
      onClose={onClose}
      className="m-auto w-[min(32rem,calc(100vw-2rem))] rounded-lg border border-line bg-surface p-0 text-fg shadow-xl backdrop:bg-black/50"
    >
      {open && (
        <div className="space-y-4 px-5 py-4">
          <h2 id={titleId} className="text-base font-semibold">
            {title}
          </h2>
          {children}
          {footer !== undefined && <div className="flex flex-wrap justify-end gap-2">{footer}</div>}
        </div>
      )}
    </dialog>
  );
}

export interface ConfirmDialogProps {
  open: boolean;
  title: ReactNode;
  children?: ReactNode;
  confirmLabel: string;
  /** "danger" for what can't be undone. */
  tone?: "primary" | "danger";
  pending?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}

/** Asks before doing something; Cancel has the focus first. */
export function ConfirmDialog({
  open,
  title,
  children,
  confirmLabel,
  tone = "danger",
  pending = false,
  onConfirm,
  onCancel,
}: ConfirmDialogProps) {
  return (
    <Dialog
      open={open}
      title={title}
      onClose={onCancel}
      footer={
        <>
          <Button data-autofocus onClick={onCancel} disabled={pending}>
            Cancel
          </Button>
          <Button variant={tone} onClick={onConfirm} disabled={pending}>
            {pending && <Spinner />}
            {confirmLabel}
          </Button>
        </>
      }
    >
      {children}
    </Dialog>
  );
}
