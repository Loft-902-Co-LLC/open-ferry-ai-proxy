import { render } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { BreakableText } from "./BreakableText";

/** The text between each <wbr>, in order. */
function pieces(container: HTMLElement): string[] {
  return container.innerHTML.split("<wbr>");
}

describe("BreakableText", () => {
  it("lets a path break after each slash, and copies whole", () => {
    const { container } = render(<BreakableText text="/v1/chat/completions?stream=true&n=1" />);
    expect(container).toHaveTextContent("/v1/chat/completions?stream=true&n=1", {
      normalizeWhitespace: false,
    });
    expect(pieces(container)).toEqual([
      "/",
      "v1/",
      "chat/",
      "completions?",
      "stream=true&amp;",
      "n=1",
    ]);
  });

  it("lets a model name break after each dash or slash, never at its end", () => {
    const { container } = render(<BreakableText text="openai/gpt-5.1-codex-" kind="name" />);
    expect(container).toHaveTextContent("openai/gpt-5.1-codex-");
    expect(pieces(container)).toEqual(["openai/", "gpt-", "5.1-", "codex-"]);
  });

  it("leaves text without a joint as it is", () => {
    const { container } = render(<BreakableText text="codex" kind="name" />);
    expect(container.innerHTML).toBe("codex");
  });
});
