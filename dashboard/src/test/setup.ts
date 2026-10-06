import "@testing-library/jest-dom/vitest";
import "../lib/zod";

import { cleanup, configure } from "@testing-library/react";
import { afterEach } from "vitest";

// Tests of the build scripts run in Node, without a DOM.
const hasDom = typeof window !== "undefined";

if (hasDom) {
  installDomGaps();
  // A busy CI machine runs the tests several times slower than a quiet one.
  // Lazy pages load before the tests that show them (loadFirst.ts), outside
  // these waits.
  configure({ asyncUtilTimeout: 3000 });
}

afterEach(() => {
  if (hasDom) {
    cleanup();
    window.sessionStorage.clear();
  }
});

/**
 * What jsdom lacks and the app uses: <dialog>'s modal methods,
 * scrollIntoView, and ResizeObserver, which charts size themselves with.
 * Stand-ins that do what the tests need: a dialog opens and closes, firing
 * "close"; scrolling does nothing; a chart gets a fixed size.
 */
function installDomGaps() {
  const dialog = window.HTMLDialogElement.prototype as Partial<HTMLDialogElement>;
  if (typeof dialog.showModal !== "function") {
    Object.assign(window.HTMLDialogElement.prototype, {
      show(this: HTMLDialogElement) {
        this.setAttribute("open", "");
      },
      showModal(this: HTMLDialogElement) {
        this.setAttribute("open", "");
      },
      close(this: HTMLDialogElement, returnValue?: string) {
        if (!this.hasAttribute("open")) {
          return;
        }
        this.removeAttribute("open");
        if (returnValue !== undefined) {
          this.returnValue = returnValue;
        }
        this.dispatchEvent(new Event("close"));
      },
    });
  }

  const element = window.Element.prototype as Partial<Element>;
  if (typeof element.scrollIntoView !== "function") {
    // jsdom lays nothing out, so there is nowhere to scroll.
    window.Element.prototype.scrollIntoView = () => undefined;
  }

  if (typeof window.ResizeObserver !== "function") {
    class FixedSizeObserver implements ResizeObserver {
      readonly #callback: ResizeObserverCallback;
      constructor(callback: ResizeObserverCallback) {
        this.#callback = callback;
      }
      observe(target: Element) {
        const rect = { width: 800, height: 256, top: 0, left: 0, bottom: 256, right: 800, x: 0, y: 0 };
        const entry = {
          target,
          contentRect: { ...rect, toJSON: () => rect },
          borderBoxSize: [{ inlineSize: 800, blockSize: 256 }],
          contentBoxSize: [{ inlineSize: 800, blockSize: 256 }],
          devicePixelContentBoxSize: [{ inlineSize: 800, blockSize: 256 }],
        } as unknown as ResizeObserverEntry;
        this.#callback([entry], this);
      }
      unobserve() {
        // Nothing is watched.
      }
      disconnect() {
        // Nothing is watched.
      }
    }
    window.ResizeObserver = FixedSizeObserver;
  }
}
