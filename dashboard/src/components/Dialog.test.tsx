import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { ConfirmDialog, Dialog } from "./Dialog";

/** What the browser does on Escape: a cancelable "cancel" event, then it closes. */
function pressEscape(dialog: HTMLElement): boolean {
  return fireEvent(dialog, new Event("cancel", { cancelable: true }));
}

describe("Dialog", () => {
  it("leaves Escape to the browser, which closes it, unless the owner takes it", () => {
    const onClose = vi.fn();
    render(
      <Dialog open title="Plain" onClose={onClose}>
        <p>Text</p>
      </Dialog>,
    );
    const dialog = screen.getByRole("dialog", { name: "Plain" });
    // Not prevented: the browser goes on to close it.
    expect(pressEscape(dialog)).toBe(true);
  });

  it("hands Escape to onCancel, and stays open until the owner closes it", () => {
    const onClose = vi.fn();
    const onCancel = vi.fn();
    const { rerender } = render(
      <Dialog open title="Decides" onClose={onClose} onCancel={onCancel}>
        <p>Text</p>
      </Dialog>,
    );
    const dialog = screen.getByRole("dialog", { name: "Decides" });
    // Prevented: the browser does not close it.
    expect(pressEscape(dialog)).toBe(false);
    expect(onCancel).toHaveBeenCalledTimes(1);
    expect(onClose).not.toHaveBeenCalled();
    expect(dialog).toHaveAttribute("open");

    rerender(
      <Dialog open={false} title="Decides" onClose={onClose} onCancel={onCancel}>
        <p>Text</p>
      </Dialog>,
    );
    expect(dialog).not.toHaveAttribute("open");
  });
});

describe("ConfirmDialog", () => {
  it("prevents Escape while pending, then lets idle Escape cancel", () => {
    const onCancel = vi.fn();
    const onConfirm = vi.fn();
    const props = {
      title: "Delete file?",
      confirmLabel: "Delete file",
      onCancel,
      onConfirm,
    };
    const { rerender } = render(<ConfirmDialog {...props} open pending />);
    const dialog = screen.getByRole("dialog", { name: "Delete file?" });

    expect(pressEscape(dialog)).toBe(false);
    expect(onCancel).not.toHaveBeenCalled();
    expect(onConfirm).not.toHaveBeenCalled();
    expect(dialog).toHaveAttribute("open");
    expect(screen.getByRole("button", { name: "Cancel" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Delete file" })).toBeDisabled();

    rerender(<ConfirmDialog {...props} open pending={false} />);
    expect(screen.getByRole("button", { name: "Cancel" })).toBeEnabled();
    expect(screen.getByRole("button", { name: "Delete file" })).toBeEnabled();
    expect(pressEscape(dialog)).toBe(true);
    // jsdom doesn't perform the browser's default close after cancel.
    fireEvent(dialog, new Event("close"));
    expect(onCancel).toHaveBeenCalledTimes(1);
    expect(onConfirm).not.toHaveBeenCalled();
  });

  it("lets the owner close a pending dialog when the action completes", () => {
    const onCancel = vi.fn();
    const onConfirm = vi.fn();
    const props = {
      title: "Delete file?",
      confirmLabel: "Delete file",
      onCancel,
      onConfirm,
    };
    const { rerender } = render(<ConfirmDialog {...props} open pending />);
    const dialog = screen.getByRole("dialog", { name: "Delete file?" });

    rerender(<ConfirmDialog {...props} open={false} pending={false} />);
    expect(dialog).not.toHaveAttribute("open");
    expect(onCancel).toHaveBeenCalledTimes(1);
    expect(onConfirm).not.toHaveBeenCalled();
  });
});
