import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { Table, Td } from "./Table";

describe("Table", () => {
  it("is named by its caption, and scrolls inside a positioned wrapper", () => {
    render(
      <Table caption="Prices">
        <tbody>
          <tr>
            <Td>
              Change<span className="sr-only"> the price</span>
            </Td>
          </tr>
        </tbody>
      </Table>,
    );
    const table = screen.getByRole("table", { name: "Prices" });
    // sr-only text is absolutely placed; a wrapper that isn't positioned lets
    // it widen the page at 390 px.
    expect(table.parentElement).toHaveClass("relative", "overflow-x-auto");
  });
});
