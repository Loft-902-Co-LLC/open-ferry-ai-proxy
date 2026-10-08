import { screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { CHECK_PATH } from "../api/signIn";
import { mockApi, route } from "../test/mockApi";
import { renderApp } from "../test/renderApp";

const REPO = "https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy";

describe("the About page", () => {
  it("links to the README, the docs and the dashboard's API", async () => {
    mockApi(route("GET", CHECK_PATH, { json: {} }));
    renderApp("/about");
    expect(await screen.findByRole("link", { name: "open-ferry's README" })).toHaveAttribute(
      "href",
      REPO,
    );
    expect(screen.getByRole("link", { name: "its docs" })).toHaveAttribute(
      "href",
      `${REPO}/tree/main/docs`,
    );
    expect(screen.getByRole("link", { name: "dashboard-api.md" })).toHaveAttribute(
      "href",
      `${REPO}/blob/main/docs/dashboard-api.md`,
    );
  });
});
