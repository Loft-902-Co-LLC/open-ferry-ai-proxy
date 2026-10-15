import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { Dialog } from "./Dialog";

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
