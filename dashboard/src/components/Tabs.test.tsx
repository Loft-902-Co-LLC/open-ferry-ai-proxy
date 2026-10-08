import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it } from "vitest";

import { Tabs } from "./Tabs";

const ITEMS = [
  { id: "one", label: "First" },
  { id: "two", label: "Second" },
  { id: "three", label: "Third" },
];

function Example() {
  const [selected, setSelected] = useState("one");
  return (
    <Tabs label="Example" items={ITEMS} selected={selected} onSelect={setSelected}>
      <p>Panel {selected}</p>
    </Tabs>
  );
}

describe("Tabs", () => {
  it("moves and selects with the arrow keys, Home and End", async () => {
    const user = userEvent.setup();
    render(<Example />);
    expect(screen.getByRole("tablist", { name: "Example" })).toBeInTheDocument();
    const first = screen.getByRole("tab", { name: "First" });
    expect(first).toHaveAttribute("aria-selected", "true");

    await user.tab();
    expect(first).toHaveFocus();
    await user.keyboard("{ArrowRight}");
    expect(screen.getByRole("tab", { name: "Second" })).toHaveFocus();
    expect(screen.getByRole("tab", { name: "Second" })).toHaveAttribute("aria-selected", "true");
    expect(screen.getByRole("tabpanel", { name: "Second" })).toHaveTextContent("Panel two");

    await user.keyboard("{End}");
    expect(screen.getByRole("tab", { name: "Third" })).toHaveFocus();
    await user.keyboard("{ArrowRight}");
    expect(first).toHaveFocus();
    await user.keyboard("{ArrowLeft}");
    expect(screen.getByRole("tab", { name: "Third" })).toHaveFocus();
    await user.keyboard("{Home}");
    expect(first).toHaveFocus();
  });

  it("keeps only the selected tab in the tab order, then the panel", async () => {
    const user = userEvent.setup();
    render(<Example />);
    await user.tab();
    expect(screen.getByRole("tab", { name: "First" })).toHaveFocus();
    await user.tab();
    expect(screen.getByRole("tabpanel", { name: "First" })).toHaveFocus();
  });
});
