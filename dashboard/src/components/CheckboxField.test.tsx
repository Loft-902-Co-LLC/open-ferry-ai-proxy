import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { Checkbox, CheckboxField } from "./CheckboxField";

describe("Checkbox", () => {
  it("is a native checkbox that its label names and toggles", async () => {
    const user = userEvent.setup();
    render(
      <label>
        <Checkbox /> Follow
      </label>,
    );
    const box = screen.getByRole("checkbox", { name: "Follow" });
    expect(box).not.toBeChecked();
    await user.click(screen.getByText("Follow"));
    expect(box).toBeChecked();
    await user.keyboard(" ");
    expect(box).not.toBeChecked();
    expect(box).toHaveFocus();
  });

  it("can be turned off", () => {
    render(
      <label>
        <Checkbox disabled /> Wrap
      </label>,
    );
    expect(screen.getByRole("checkbox", { name: "Wrap" })).toBeDisabled();
  });
});

describe("CheckboxField", () => {
  it("names the box with its label and describes it with its hint", async () => {
    const user = userEvent.setup();
    render(<CheckboxField label="Debug" hint="Logs more detail." />);
    const box = screen.getByRole("checkbox", { name: "Debug" });
    expect(box).toHaveAccessibleDescription("Logs more detail.");
    await user.click(box);
    expect(box).toBeChecked();
  });
});
