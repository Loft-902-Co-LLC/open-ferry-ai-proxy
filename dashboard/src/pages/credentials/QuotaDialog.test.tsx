import { act, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { AUTH_FILES, QUOTA_FETCH, type QuotaAnswer } from "../../api/credentials";
import { credential, credentialList, cooldown, quotaCheck } from "../../test/fixtures";
import { mockApi, route } from "../../test/mockApi";
import { renderApp } from "../../test/renderApp";
import { canReset } from "./credentialStates";
import { quotaCooldownConflict } from "./QuotaDialog";

const available: QuotaAnswer = { groups: [{ buckets: [{ window: "session", remainingFraction: 1 }] }] };

describe("fresh quota and cached cooldown", () => {
  it("requires reported capacity in every bucket and a quota rest", () => {
    const resting = credential({ cooldowns: [cooldown("credential_quota", 300)] });
    expect(quotaCooldownConflict(resting, available)).toBe(true);
    for (const remainingFraction of [0, -1, 2, Number.NaN, Number.POSITIVE_INFINITY]) {
      expect(quotaCooldownConflict(resting, {
        groups: [{ buckets: [
          { window: "session", remainingFraction: 1 },
          { window: "weekly", remainingFraction },
        ] }],
      })).toBe(false);
    }
    expect(quotaCooldownConflict(resting, {})).toBe(false);
    expect(quotaCooldownConflict(resting, { groups: [{ buckets: [] }] })).toBe(false);
    for (const reason of ["unauthorized", "transient_error"] as const) {
      expect(quotaCooldownConflict(credential({ cooldowns: [cooldown(reason, 300)] }), available)).toBe(false);
    }
    const due = credential({ quota_checks: [quotaCheck("due")] });
    expect(quotaCooldownConflict(due, available)).toBe(true);
    expect(canReset(due)).toBe(true);
  });

  it.each([false, true])("waits for the credential refresh before showing a conflict (recovered: %s)", async (recovered) => {
    let file = credential({ supports_quota: true, cooldowns: [cooldown("quota", 300)] });
    let reads = 0;
    let finishRefresh: (() => void) | undefined;
    const api = mockApi(
      route("GET", AUTH_FILES, async () => {
        reads += 1;
        if (reads > 1) {
          await new Promise<void>((resolve) => { finishRefresh = resolve; });
          if (recovered) file = credential({ supports_quota: true });
        }
        return { json: credentialList([file]) };
      }),
      route("POST", QUOTA_FETCH, { json: available }),
    );
    const { user } = renderApp("/credentials");
    const item = await screen.findByRole("article", { name: "ada@example.com" });
    await user.click(within(item).getByRole("button", { name: "Check quota of ada@example.com" }));
    await waitFor(() => { expect(finishRefresh).toBeDefined(); });
    expect(screen.queryByText("Proxy quota cooldown still cached")).toBeNull();
    act(() => { finishRefresh?.(); });
    if (recovered) {
      await screen.findByRole("button", { name: "1 ready" });
      expect(screen.queryByText("Proxy quota cooldown still cached")).toBeNull();
    } else {
      expect(await screen.findByText("Proxy quota cooldown still cached")).toBeVisible();
      const dialog = screen.getByRole("dialog", { name: "Quota of ada@example.com" });
      expect(dialog).toHaveTextContent("The read may not confirm the limit that caused it");
      expect(dialog).toHaveTextContent("Unknown or model-specific limits");
    }
    expect(api.callsTo("POST", QUOTA_FETCH)[0]?.json()).toEqual({ auth_index: file.auth_index });
    expect(api.callsTo("GET", AUTH_FILES)).toHaveLength(2);
  });

  it.each(["exhausted", "empty", "failed"])("does not show a conflict after a %s quota read", async (answer) => {
    const api = mockApi(
      route("GET", AUTH_FILES, { json: credentialList([
        credential({ supports_quota: true, cooldowns: [cooldown("quota", 300)] }),
      ]) }),
      route("POST", QUOTA_FETCH, answer === "failed" ? { status: 502, json: { error: "quota failed" } } : {
        json: answer === "empty" ? {} : { groups: [{ buckets: [
          { window: "session", remainingFraction: 1 },
          { window: "weekly", remainingFraction: 0 },
        ] }] },
      }),
    );
    const { user } = renderApp("/credentials");
    const item = await screen.findByRole("article", { name: "ada@example.com" });
    await user.click(within(item).getByRole("button", { name: "Check quota of ada@example.com" }));
    await waitFor(() => {
      expect(screen.queryByText("Asking the provider…")).toBeNull();
    });
    expect(screen.queryByText("Proxy quota cooldown still cached")).toBeNull();
    expect(api.callsTo("GET", AUTH_FILES)).toHaveLength(answer === "failed" ? 1 : 2);
  });
});
