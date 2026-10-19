import { act, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { pollFailuresExhausted, switchInProgress } from "../../api/updateStatus";
import { CHECK_PATH } from "../../api/signIn";
import { UPDATE, UPDATE_CHECK, UPDATE_INSTALL, UPDATE_ROLLBACK, type UpdateStatus } from "../../api/update";
import { supervisorStatus, updateStatus } from "../../test/fixtures";
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

describe("supervised updates", () => {
  it.each(["staged", "available"])("confirms an install when a release is %s", async (release) => {
    const api = server(updateStatus({
      supervisor: supervisorStatus(),
      latest_version: "0.2.0", update_available: true,
      staged_version: release === "staged" ? "0.2.0" : null,
    }));
    api.use(route("POST", UPDATE_INSTALL, () => {
      api.use(route("GET", UPDATE, { json: updateStatus({
        supervisor: supervisorStatus({ phase: "loading", switch: { id: "install-1", how: "update", from: "0.1.0", to: "0.2.0" } }),
      }) }));
      return { status: 202, json: { accepted: true, id: "install-1" } };
    }));
    const { card, user } = await openUpdates();
    await user.click(await within(card).findByRole("button", { name: "Install now" }));
    expect(api.callsTo("POST", UPDATE_INSTALL)).toHaveLength(0);
    let dialog = screen.getByRole("dialog");
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    expect(api.callsTo("POST", UPDATE_INSTALL)).toHaveLength(0);
    await user.click(within(card).getByRole("button", { name: "Install now" }));
    dialog = screen.getByRole("dialog");
    await user.click(within(dialog).getByRole("button", { name: "Install now" }));
    expect(await within(card).findByText("Switch: loading")).toBeVisible();
    expect(api.callsTo("POST", UPDATE_INSTALL)).toHaveLength(1);
    expect(within(card).queryByRole("button", { name: "Install now" })).not.toBeInTheDocument();
    expect(api.unhandled).toEqual([]);
  });

  it("confirms a rollback to the supervisor's target during a watch", async () => {
    const api = server(updateStatus({ supervisor: supervisorStatus({
      phase: "watching", running_version: "0.2.0", rollback_target: "0.1.0",
      watch_until: "2026-10-18T16:10:00Z",
    }) }));
    api.use(route("POST", UPDATE_ROLLBACK, () => {
      api.use(route("GET", UPDATE, { json: updateStatus({ supervisor: supervisorStatus({
        last_switch: { how: "rollback", auto: null, from: "0.2.0", to: "0.1.0", result: "rolled-back", reason: null, at: "2026-10-18T16:01:00Z" },
      }) }) }));
      return { status: 202, json: { accepted: true, id: "rollback-1" } };
    }));
    const { card, user } = await openUpdates();
    await user.click(await within(card).findByRole("button", { name: "Roll back" }));
    const dialog = screen.getByRole("dialog", { name: "Roll back to 0.1.0?" });
    expect(api.callsTo("POST", UPDATE_ROLLBACK)).toHaveLength(0);
    await user.click(within(dialog).getByRole("button", { name: "Roll back" }));
    expect(await within(card).findByText("Rolled back to 0.1.0")).toBeVisible();
    expect(api.callsTo("POST", UPDATE_ROLLBACK)).toHaveLength(1);
    expect(card).not.toHaveTextContent("Automatic switch");
  });

  it.each(["waiting-for-signins", "loading", "handing-over", "taking-over", "watching"])("shows the %s phase", async (phase) => {
    server(updateStatus({ supervisor: supervisorStatus({ phase }) }));
    const { card } = await openUpdates();
    expect(await within(card).findByText(`Switch: ${phase.replaceAll("-", " ")}`)).toBeVisible();
    expect(within(card).queryByRole("button", { name: "Install now" })).not.toBeInTheDocument();
  });

  it.each(["preparing", "refused", "failed", "unknown"] as const)("shows a %s request", async (state) => {
    server(updateStatus({
      supervisor: supervisorStatus(),
      switch_request: { action: "install", state, id: null, pending: state === "preparing", reason: state === "preparing" ? null : "sign-ins have not finished" },
    }));
    const { card } = await openUpdates();
    expect(await within(card).findByText(state === "preparing" ? "Preparing the switch" : "sign-ins have not finished")).toBeVisible();
  });

  it("shows an accepted request before the supervisor publishes its phase", async () => {
    server(updateStatus({ supervisor: supervisorStatus(), switch_request: {
      action: "install", state: "accepted", id: "install-1", reason: null, pending: true,
    } }));
    const { card } = await openUpdates();
    expect(await within(card).findByText("Switch accepted")).toBeVisible();
  });

  it("shows why switching is disabled", async () => {
    server(updateStatus({ update_available: true, supervisor: supervisorStatus({
      rollback_target: "0.0.9", switching_disabled: "the journal is stale",
    }) }));
    const { card } = await openUpdates();
    expect(await within(card).findByText("the journal is stale")).toBeVisible();
    expect(within(card).queryByRole("button", { name: "Install now" })).not.toBeInTheDocument();
    expect(within(card).queryByRole("button", { name: "Roll back" })).not.toBeInTheDocument();
  });

  it.each([true, false, null])("labels a switch automatic only when recorded true (%s)", async (auto) => {
    server(updateStatus({ supervisor: supervisorStatus({ last_switch: {
      how: "update", auto, from: "0.1.0", to: "0.2.0", result: "switched", reason: null, at: null,
    } }) }));
    const { card } = await openUpdates();
    await within(card).findByText("Switched to 0.2.0");
    expect(card.textContent.includes("Automatic switch")).toBe(auto === true);
  });

  it("shows an unfamiliar outcome and its reason", async () => {
    server(updateStatus({ supervisor: supervisorStatus({ last_switch: {
      how: "recovery", auto: null, from: null, to: "0.1.0", result: "returned-to-installed", reason: "the new worker exited", at: null,
    } }) }));
    const { card } = await openUpdates();
    expect(await within(card).findByText("returned to installed: 0.1.0")).toBeVisible();
    expect(card).toHaveTextContent("the new worker exited");
  });

  it("polls through handover failures quietly and stops after the switch", async () => {
    let reads = 0;
    const api = mockApi(
      route("GET", CHECK_PATH, { json: {} }),
      route("GET", UPDATE, () => {
        reads += 1;
        if (reads === 2) { throw new TypeError("handover"); }
        return { json: updateStatus({ supervisor: supervisorStatus({
          phase: reads === 1 ? "handing-over" : "idle",
          last_switch: reads === 1 ? null : { how: "update", auto: null, from: "0.1.0", to: "0.2.0", result: "switched", reason: null, at: null },
        }) }) };
      }),
    );
    const { card } = await openUpdates();
    await within(card).findByText("Switch: handing over");
    expect(await within(card).findByText("Switched to 0.2.0", undefined, { timeout: 8_000 })).toBeVisible();
    expect(within(card).queryByRole("button", { name: "Try again" })).not.toBeInTheDocument();
    const finishedReads = api.callsTo("GET", UPDATE).length;
    await new Promise((resolve) => setTimeout(resolve, 2_100));
    expect(api.callsTo("GET", UPDATE)).toHaveLength(finishedReads);
  }, 12_000);
});

describe("manual install retries", () => {
  it.each(["failed", "rolled back"])("warns before retrying a version that was %s", async (previous) => {
    const api = server(updateStatus({
      supervisor: supervisorStatus(), latest_version: "0.2.0", staged_version: "0.2.0", update_available: true,
      failed_versions: previous === "failed" ? ["0.2.0"] : [],
      rolled_back_version: previous === "rolled back" ? "0.2.0" : null,
    }));
    api.use(route("POST", UPDATE_INSTALL, { status: 409, json: { error: "update_busy", message: "busy, try again shortly" } }));
    const { card, user } = await openUpdates();
    await user.click(await within(card).findByRole("button", { name: "Install now" }));
    const dialog = screen.getByRole("dialog");
    expect(dialog).toHaveTextContent(previous === "failed" ? "failed here before. Install it again?" : "was rolled back from. Install it again?");
    await user.click(within(dialog).getByRole("button", { name: "Install now" }));
    expect(await within(card).findByText("busy, try again shortly")).toBeVisible();
    expect(api.callsTo("POST", UPDATE_INSTALL)).toHaveLength(1);
  });

  it("does not offer a staged version already running under the supervisor", async () => {
    server(updateStatus({
      installed_version: "0.1.0", latest_version: "0.2.0", staged_version: "0.2.0", update_available: true,
      supervisor: supervisorStatus({ running_version: "0.2.0" }),
    }));
    const { card } = await openUpdates();
    await within(card).findByText("Running", { selector: "dt" });
    expect(within(card).queryByRole("button", { name: "Install now" })).not.toBeInTheDocument();
    expect(card).not.toHaveTextContent("ready to install");
  });
});

describe("switch failures", () => {
  it("defensively shows a direct ended reply as failed", async () => {
    const status = updateStatus({ supervisor: supervisorStatus(), staged_version: "0.2.0" });
    const api = server(status);
    api.use(route("POST", UPDATE_INSTALL, () => {
      api.use(route("GET", UPDATE, { json: {
        ...status,
        switch_request: { action: "install", state: "failed", id: null, reason: "the new worker exited", pending: false },
      } }));
      return { status: 202, json: { failed: true, reason: "the new worker exited" } };
    }));
    const { card, user } = await openUpdates();
    await user.click(await within(card).findByRole("button", { name: "Install now" }));
    await user.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Install now" }));
    expect(await within(card).findByText("Switch failed")).toBeVisible();
    expect(card).toHaveTextContent("the new worker exited");
    expect(card).not.toHaveTextContent("Switch refused");
    expect(api.callsTo("POST", UPDATE_INSTALL)).toHaveLength(1);
  });

  it("shows a supervisor read error instead of a cached handover phase", async () => {
    server(updateStatus({ supervisor: supervisorStatus({
      phase: "handing-over", stale: true, error: "the supervisor cannot be reached",
      switching_disabled: "a switch is already active",
    }) }));
    const { card } = await openUpdates();
    expect(await within(card).findByText("The supervisor status could not be read")).toBeVisible();
    expect(card).not.toHaveTextContent("Switch: handing over");
    expect(within(card).getByRole("button", { name: "Try again" })).toBeVisible();
  });
});

describe("uncertain switch observation", () => {
  it("keeps the unknown outcome after status is read and permits an explicit retry", async () => {
    let reads = 0;
    const api = mockApi(
      route("GET", CHECK_PATH, { json: {} }),
      route("GET", UPDATE, () => {
        reads += 1;
        return { json: updateStatus({
          supervisor: supervisorStatus(), staged_version: "0.2.0",
          switch_request: { action: "install", state: "unknown", id: null, reason: "the outcome is unknown", pending: reads === 1 },
        }) };
      }),
    );
    const { card, user } = await openUpdates();
    await within(card).findByText("the outcome is unknown");
    expect(within(card).queryByRole("button", { name: "Install now" })).not.toBeInTheDocument();
    await user.click(within(card).getByRole("button", { name: "Read status" }));
    expect(await within(card).findByRole("button", { name: "Install now" }, { timeout: 5_000 })).toBeEnabled();
    expect(card).toHaveTextContent("The switch could not be confirmed");
    expect(card).not.toHaveTextContent("Switch failed");
    expect(api.callsTo("POST", UPDATE_INSTALL)).toHaveLength(0);
    const observedReads = reads;
    await new Promise((resolve) => setTimeout(resolve, 2_100));
    expect(reads).toBe(observedReads);
  }, 8_000);
});


describe("handover poll failures", () => {
  it("shows a lasting outage after five failed polls and stops polling", async () => {
    let reads = 0;
    const api = mockApi(
      route("GET", CHECK_PATH, { json: {} }),
      route("GET", UPDATE, () => {
        if (++reads > 1) throw new TypeError("offline");
        return { json: updateStatus({ supervisor: supervisorStatus({ phase: "handing-over" }) }) };
      }),
    );
    const { card } = await openUpdates();
    await within(card).findByText("Switch: handing over");
    await within(card).findByRole("button", { name: "Try again" }, { timeout: 12_000 });
    expect(reads).toBe(6);
    await new Promise((resolve) => setTimeout(resolve, 2_100));
    expect(api.callsTo("GET", UPDATE)).toHaveLength(6);
  });

  it("also ends the quiet period at thirty seconds", () => {
    expect(pollFailuresExhausted(4, 1_000, 30_999)).toBe(false);
    expect(pollFailuresExhausted(4, 1_000, 31_000)).toBe(true);
    expect(pollFailuresExhausted(5, 1_000, 1_001)).toBe(true);
  });
});


it("does not keep unknown or expired cached phases running", () => {
  expect(switchInProgress(updateStatus({ supervisor: supervisorStatus({ phase: "unknown" }) }))).toBe(false);
  expect(switchInProgress(updateStatus({ switch_request: { action: "install", state: "unknown", pending: true, id: null, reason: "unknown" } }))).toBe(false);
  for (const stale of [{ failed_reads: 5, snapshot_age_seconds: 1 }, { failed_reads: 1, snapshot_age_seconds: 30 }]) {
    expect(switchInProgress(updateStatus({ supervisor: supervisorStatus({ phase: "handing-over", stale: true, ...stale }) }))).toBe(false);
  }
});


it.each(["request", "outcome"])("clears a POST error when status supplies a newer %s", async (newer) => {
  const original = updateStatus({ supervisor: supervisorStatus(), staged_version: "0.2.0" });
  const api = server(original);
  api.use(
    route("POST", UPDATE_INSTALL, { status: 409, json: { error: "update_busy", message: "busy, try again shortly" } }),
    route("POST", UPDATE_CHECK, { status: 202, json: { check: "started" } }),
  );
  const { card, user } = await openUpdates();
  await user.click(await within(card).findByRole("button", { name: "Install now" }));
  await user.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Install now" }));
  expect(await within(card).findByText("The switch request failed")).toBeVisible();
  // Reading the same old status did not clear the POST error.
  api.use(route("GET", UPDATE, { json: {
    ...original,
    switch_request: newer === "request" ? { action: "install", state: "failed", id: null, pending: false, reason: "preparation failed" } : null,
    supervisor: supervisorStatus({ last_switch: newer === "outcome" ? { how: "update", auto: null, from: "0.1.0", to: "0.2.0", result: "switched", reason: null, at: null } : null }),
  } }));
  await user.click(within(card).getByRole("button", { name: "Check now" }));
  await within(card).findByText(newer === "request" ? "Switch failed" : "Switched to 0.2.0");
  expect(within(card).queryByText("The switch request failed")).not.toBeInTheDocument();
});


it("cancels an in-flight poll before publishing optimistic preparation", async () => {
  const original = updateStatus({ supervisor: supervisorStatus(), staged_version: "0.2.0" });
  let reads = 0;
  let releasePoll!: (reply: MockReply) => void;
  let releasePost!: (reply: MockReply) => void;
  const api = mockApi(
    route("GET", CHECK_PATH, { json: {} }),
    route("GET", UPDATE, () => ++reads === 2 ? new Promise<MockReply>((resolve) => { releasePoll = resolve; }) : { json: original }),
    route("POST", UPDATE_CHECK, { status: 202, json: { check: "started" } }),
    route("POST", UPDATE_INSTALL, () => new Promise<MockReply>((resolve) => { releasePost = resolve; })),
  );
  const { card, user } = await openUpdates();
  await user.click(await within(card).findByRole("button", { name: "Check now" }));
  await waitFor(() => { expect(reads).toBe(2); });
  await user.click(within(card).getByRole("button", { name: "Install now" }));
  await user.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Install now" }));
  await waitFor(() => { expect(api.callsTo("POST", UPDATE_INSTALL)).toHaveLength(1); });
  await within(card).findByText("Preparing the switch");
  await act(async () => { releasePoll({ json: original }); await Promise.resolve(); });
  expect(within(card).getByText("Preparing the switch")).toBeVisible();
  await act(async () => { releasePost({ status: 202, json: { accepted: true, id: "install-1" } }); await Promise.resolve(); });
});


it("labels a check recorded before the current worker version", async () => {
  server(updateStatus({ supervisor: supervisorStatus({ running_version: "0.2.0" }) }));
  const { card } = await openUpdates();
  await within(card).findByText("Running", { selector: "dt" });
  expect(fact(card, "Running")).toBe("0.2.0");
  expect(fact(card, "Latest release")).toBe("0.1.0 (last checked)");
  expect(fact(card, "Last check")).toMatch(/checked before the running version changed$/);
  expect(fact(card, "Last check")).not.toContain("up to date");
});


it("keeps an unanswered POST unconfirmed until the server reports even identical preparation", async () => {
  const original = updateStatus({ supervisor: supervisorStatus(), staged_version: "0.2.0" });
  let posted = false;
  let readsAfterPost = 0;
  const api = mockApi(
    route("GET", CHECK_PATH, { json: {} }),
    route("GET", UPDATE, () => {
      if (!posted) return { json: original };
      if (++readsAfterPost === 1) throw new TypeError("handover");
      return { json: { ...original, switch_request: { action: "install", state: "preparing", id: null, reason: null, pending: true } } };
    }),
    route("POST", UPDATE_INSTALL, () => { posted = true; throw new TypeError("no reply"); }),
  );
  const { card, user } = await openUpdates();
  await user.click(await within(card).findByRole("button", { name: "Install now" }));
  await user.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Install now" }));
  await within(card).findByText("The switch could not be confirmed");
  expect(card).not.toHaveTextContent("The switch request failed");
  await waitFor(() => { expect(readsAfterPost).toBeGreaterThan(1); }, { timeout: 5_000 });
  await waitFor(() => { expect(card).not.toHaveTextContent("The switch could not be confirmed"); });
  expect(within(card).getByText("Preparing the switch")).toBeVisible();
  expect(api.callsTo("POST", UPDATE_INSTALL)).toHaveLength(1);
});
