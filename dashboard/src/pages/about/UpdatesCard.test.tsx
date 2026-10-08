import { screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { CHECK_PATH } from "../../api/signIn";
import { UPDATE, UPDATE_CHECK, type UpdateStatus } from "../../api/update";
import { updateStatus } from "../../test/fixtures";
import { mockApi, route, type MockReply } from "../../test/mockApi";
import { renderApp } from "../../test/renderApp";

/** The About page, with `GET /update` answering `status`. */
function server(status: UpdateStatus | MockReply) {
  return mockApi(
    route("GET", CHECK_PATH, { json: {} }),
    route("GET", UPDATE, "mode" in status ? { json: status } : status),
  );
}

async function openUpdates() {
  const view = renderApp("/about");
  const card = await screen.findByRole("region", { name: "Updates" });
  return { ...view, card };
}

/** A fact's value, by its term. */
function fact(card: HTMLElement, term: string): string | null {
  const dt = within(card).getByText(term, { selector: "dt" });
  return dt.nextElementSibling?.textContent ?? null;
}

describe("the Updates card", () => {
  it("shows what updates are doing", async () => {
    const api = server(updateStatus());
    const { card } = await openUpdates();
    await within(card).findByText("Running", { selector: "dt" });
    expect(fact(card, "Updates")).toBe("On set in config.yaml");
    expect(fact(card, "Running")).toBe("0.1.0");
    expect(fact(card, "Latest release")).toBe("0.1.0");
    expect(fact(card, "Last check")).toMatch(/: up to date$/);
    expect(fact(card, "Checks every")).toBe("6 h");
    expect(fact(card, "Updates itself")).toBe("yes");
    expect(within(card).queryByText("Installed", { selector: "dt" })).not.toBeInTheDocument();
    expect(within(card).getByRole("button", { name: "Check now" })).toBeEnabled();
    expect(within(card).getByRole("link", { name: "Settings" })).toHaveAttribute(
      "href",
      "/settings",
    );
    expect(api.unhandled).toEqual([]);
  });

  it("checks now, and reads the status again until the check ends", async () => {
    let reads = 0;
    let checked = false;
    const api = mockApi(
      route("GET", CHECK_PATH, { json: {} }),
      route("GET", UPDATE, () => {
        if (!checked) {
          return { json: updateStatus() };
        }
        reads += 1;
        return {
          json:
            reads === 1
              ? updateStatus({ checking: true })
              : updateStatus({
                  latest_version: "0.2.0",
                  update_available: true,
                  staged_version: "0.2.0",
                  last_result: "staged",
                }),
        };
      }),
      route("POST", UPDATE_CHECK, () => {
        checked = true;
        return { status: 202, json: { check: "started" } };
      }),
    );
    const { card, user } = await openUpdates();
    await user.click(await within(card).findByRole("button", { name: "Check now" }));
    expect(await within(card).findByRole("button", { name: "Checking…" })).toBeDisabled();
    expect(
      await within(card).findByText("open-ferry 0.2.0 is ready to install", undefined, {
        timeout: 5_000,
      }),
    ).toBeVisible();
    expect(within(card).getByRole("button", { name: "Check now" })).toBeEnabled();
    expect(card).toHaveTextContent("run open-ferry update on the server's computer");
    expect(fact(card, "Latest release")).toBe("0.2.0");
    expect(api.callsTo("POST", UPDATE_CHECK)).toHaveLength(1);
    expect(reads).toBe(2);
    expect(api.unhandled).toEqual([]);
  });

  it("offers no check while updates are off, and says what turned them off", async () => {
    server(updateStatus({ mode: "off", mode_source: "environment", updates: "off", next_check: null }));
    const { card } = await openUpdates();
    await within(card).findByText("Running", { selector: "dt" });
    expect(fact(card, "Updates")).toBe("Off set by OPEN_FERRY_SELF_UPDATE in the server's environment");
    expect(fact(card, "Next check")).toBe("none planned");
    expect(card).toHaveTextContent("Updates are off, so the server makes no update request.");
    expect(within(card).queryByRole("button", { name: "Check now" })).not.toBeInTheDocument();
  });

  it("says when updates were turned off before a check could start", async () => {
    const api = server(updateStatus());
    api.use(
      route("POST", UPDATE_CHECK, {
        status: 409,
        json: { error: "updates_off", message: "updates are off" },
      }),
    );
    const { card, user } = await openUpdates();
    await user.click(await within(card).findByRole("button", { name: "Check now" }));
    expect(
      await within(card).findByText(
        "Nothing was checked: updates were turned off before the check could start.",
      ),
    ).toBeVisible();
  });

  it("says when the server runs no update checks", async () => {
    const api = server({
      status: 503,
      json: { error: "updates_unavailable", message: "this server doesn't check for updates" },
    });
    const { card } = await openUpdates();
    expect(
      await within(card).findByText(/This server doesn't check for updates/),
    ).toBeVisible();
    expect(within(card).queryByRole("button", { name: "Check now" })).not.toBeInTheDocument();
    // Not retried: it won't change until the server restarts.
    expect(api.callsTo("GET", UPDATE)).toHaveLength(1);
  });

  it("says when a build trusts no release key, or a restart is needed", async () => {
    server(
      updateStatus({
        trusts_release_key: false,
        installed_version: "0.2.0",
        restart_needed: true,
        previous_version: "0.1.0",
      }),
    );
    const { card } = await openUpdates();
    expect(await within(card).findByText("This build trusts no release key")).toBeVisible();
    expect(within(card).getByText("Restart the server to run 0.2.0")).toBeVisible();
    expect(fact(card, "Installed")).toBe("0.2.0");
    expect(fact(card, "Kept for rollback")).toBe("0.1.0");
    expect(within(card).queryByRole("button", { name: "Check now" })).not.toBeInTheDocument();
  });

  it("says why an install doesn't update itself when a release is out", async () => {
    server(
      updateStatus({
        latest_version: "0.2.0",
        update_available: true,
        last_result: "cannot-update",
        can_update_itself: false,
        why_not: "open-ferry runs in a container.",
        why_not_code: "container",
        failed_versions: ["0.1.5"],
      }),
    );
    const { card } = await openUpdates();
    const notice = await within(card).findByText("open-ferry 0.2.0 is out");
    expect(notice.parentElement).toHaveTextContent(
      "open-ferry runs in a container. Update it the way you installed it.",
    );
    expect(fact(card, "Updates itself")).toBe("no: open-ferry runs in a container.");
    expect(fact(card, "Skipped")).toBe("0.1.5");
  });

  it("shows a failed check and settings the server couldn't use", async () => {
    server(
      updateStatus({
        last_result: "error",
        last_error: "the server answered 500",
        notes: ["OPEN_FERRY_SELF_UPDATE=sometimes isn't a mode, so it was ignored"],
      }),
    );
    const { card } = await openUpdates();
    expect(await within(card).findByText("The last check failed")).toBeVisible();
    expect(card).toHaveTextContent("the server answered 500");
    expect(within(card).getByRole("listitem")).toHaveTextContent(
      "OPEN_FERRY_SELF_UPDATE=sometimes isn't a mode",
    );
  });
});
