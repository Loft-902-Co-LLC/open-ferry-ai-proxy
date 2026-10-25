import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { CopyButton } from "./CopyButton";

const originalClipboard = Object.getOwnPropertyDescriptor(navigator, "clipboard");
const text = "Synthetic clipboard text\n  quoted \"value\" & Unicode π";
const label = "Copy the synthetic text";

function setClipboard(clipboard: Partial<Pick<Clipboard, "writeText">> | undefined) {
  Object.defineProperty(navigator, "clipboard", { configurable: true, value: clipboard });
}

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  if (originalClipboard) {
    Object.defineProperty(navigator, "clipboard", originalClipboard);
  } else {
    Reflect.deleteProperty(navigator, "clipboard");
  }
  vi.useRealTimers();
});

describe("CopyButton", () => {
  it.each([
    { name: "the clipboard API is absent", clipboard: undefined },
    { name: "writeText is absent", clipboard: {} },
    {
      name: "writeText throws synchronously",
      clipboard: {
        writeText: () => {
          throw new Error("Synthetic synchronous failure");
        },
      },
    },
    {
      name: "writeText rejects",
      clipboard: {
        writeText: () => Promise.reject(new Error("Synthetic rejected write")),
      },
    },
  ])("shows manual-copy guidance when $name", async ({ clipboard }) => {
    setClipboard(clipboard);
    render(<CopyButton text={text} label={label} />);
    const button = screen.getByRole("button", { name: label });

    await act(async () => {
      fireEvent.click(button);
      await Promise.resolve();
    });

    expect(screen.getByRole("status")).toHaveTextContent(
      "Couldn't copy: select the text and copy it by hand.",
    );
    expect(screen.getByText("Couldn't copy", { exact: true })).toBeVisible();
    expect(button).toHaveTextContent(/^Copy$/);
    expect(screen.queryByText("Copied", { exact: true })).not.toBeInTheDocument();

    act(() => {
      vi.advanceTimersByTime(2000);
    });
    expect(screen.getByRole("status")).toBeEmptyDOMElement();
    expect(screen.queryByText("Couldn't copy", { exact: true })).not.toBeInTheDocument();
  });

  it("copies the exact text and shows success only after the write resolves", async () => {
    let finishCopy: () => void = () => undefined;
    const writeText = vi.fn<(value: string) => Promise<void>>(() => new Promise((resolve) => {
      finishCopy = resolve;
    }));
    setClipboard({ writeText });
    render(<CopyButton text={text} label={label} />);
    const button = screen.getByRole("button", { name: label });

    fireEvent.click(button);

    expect(writeText).toHaveBeenCalledExactlyOnceWith(text);
    expect(button).toHaveTextContent(/^Copy$/);
    expect(screen.getByRole("status")).toBeEmptyDOMElement();

    await act(async () => {
      finishCopy();
      await Promise.resolve();
    });

    expect(button).toHaveTextContent(/^Copied$/);
    expect(screen.getByRole("status")).toHaveTextContent(/^Copied$/);
    expect(screen.queryByText("Couldn't copy", { exact: true })).not.toBeInTheDocument();

    act(() => {
      vi.advanceTimersByTime(1999);
    });
    expect(button).toHaveTextContent(/^Copied$/);
    act(() => {
      vi.advanceTimersByTime(1);
    });
    expect(button).toHaveTextContent(/^Copy$/);
    expect(screen.getByRole("status")).toBeEmptyDOMElement();
  });
});
