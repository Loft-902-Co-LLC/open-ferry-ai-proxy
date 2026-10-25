import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
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
    const modelQuota = cooldown("quota", 300, { scope: "model", model_key: "limited-model" });
    expect(quotaCooldownConflict(credential({ cooldowns: [modelQuota] }), available)).toBe(true);
    expect(quotaCooldownConflict(credential({ cooldowns: [
      cooldown("unauthorized", 120), modelQuota,
    ] }), available)).toBe(true);
    expect(quotaCooldownConflict(credential({ cooldowns: [
      cooldown("unauthorized", 120), cooldown("transient_error", 300),
    ] }), available)).toBe(false);
    const due = credential({ quota_checks: [quotaCheck("due")] });
    expect(quotaCooldownConflict(due, available)).toBe(true);
    expect(canReset(due)).toBe(true);
  });

  it.each([
    { recovered: false, escape: false },
    { recovered: true, escape: false },
    { recovered: true, escape: true },
  ])("waits for routing refresh and preserves the dialog (recovered: $recovered, Escape: $escape)", async ({ recovered, escape }) => {
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
      const dialog = screen.getByRole("dialog", { name: "Quota of ada@example.com" });
      expect(await within(dialog).findByText(/100% left/)).toBeVisible();
      expect(screen.queryByText("Proxy quota cooldown still cached")).toBeNull();
      if (escape) {
        expect(fireEvent(dialog, new Event("cancel", { cancelable: true }))).toBe(true);
        act(() => { (dialog as HTMLDialogElement).close(); });
      } else {
        await user.click(within(dialog).getByRole("button", { name: "Close" }));
      }
      expect(screen.queryByRole("dialog", { name: "Quota of ada@example.com" })).toBeNull();
      expect(screen.getByRole("group", { name: "Credential list" })).toHaveFocus();
    } else {
      expect(await screen.findByText("Proxy quota cooldown still cached")).toBeVisible();
      const dialog = screen.getByRole("dialog", { name: "Quota of ada@example.com" });
      expect(within(dialog).getByRole("status")).toHaveTextContent("Proxy quota cooldown still cached");
      expect(dialog).toHaveTextContent("The read may not confirm the limit that caused it");
      expect(dialog).toHaveTextContent("Unknown or model-specific limits");
    }
    expect(api.callsTo("POST", QUOTA_FETCH)[0]?.json()).toEqual({ auth_index: file.auth_index });
    expect(api.callsTo("GET", AUTH_FILES)).toHaveLength(2);
  });

  it.each(["Close", "Escape"])("serializes pending probes after %s and never shows another credential's old answer", async (cancel) => {
    const first = credential({ supports_quota: true, cooldowns: [cooldown("quota", 300)] });
    const second = credential({
      id: "other", auth_index: "other-index", name: "other.json", account: "bob@example.com",
      supports_quota: true, cooldowns: [cooldown("quota", 300)],
    });
    let finishRead: (() => void) | undefined;
    const api = mockApi(
      route("GET", AUTH_FILES, { json: credentialList([first, second]) }),
      route("POST", QUOTA_FETCH, async (request) => {
        await new Promise<void>((resolve) => { finishRead = resolve; });
        const { auth_index: authIndex } = request.json() as { auth_index: string };
        return { json: { groups: [{ buckets: [{
          window: authIndex === first.auth_index ? "First account window" : "Second account window",
          remainingFraction: 0.5,
        }] }] } };
      }),
    );
    const { user } = renderApp("/credentials");
    const firstButton = await screen.findByRole("button", { name: "Check quota of ada@example.com" });
    const secondButton = screen.getByRole("button", { name: "Check quota of bob@example.com" });
    await user.click(firstButton);
    let dialog = screen.getByRole("dialog", { name: "Quota of ada@example.com" });
    if (cancel === "Escape") {
      expect(fireEvent(dialog, new Event("cancel", { cancelable: true }))).toBe(true);
      act(() => { (dialog as HTMLDialogElement).close(); });
    } else {
      await user.click(within(dialog).getByRole("button", { name: "Close" }));
    }
    expect(screen.getByRole("group", { name: "Credential list" })).toHaveFocus();
    expect(firstButton).toBeDisabled();
    expect(secondButton).toBeDisabled();
    expect(firstButton.querySelector(".lucide-loader-circle")).not.toBeNull();
    expect(secondButton.querySelector(".lucide-loader-circle")).toBeNull();
    await user.click(firstButton);
    await user.click(secondButton);
    expect(api.callsTo("POST", QUOTA_FETCH)).toHaveLength(1);
    expect(screen.queryByRole("dialog")).toBeNull();
    act(() => { finishRead?.(); });
    await waitFor(() => { expect(secondButton).toBeEnabled(); });
    await user.click(secondButton);
    dialog = screen.getByRole("dialog", { name: "Quota of bob@example.com" });
    expect(within(dialog).getByText("Asking the provider…")).toBeVisible();
    expect(within(dialog).queryByText(/First account window/)).toBeNull();
    await user.click(within(dialog).getByRole("button", { name: "Close" }));
    expect(firstButton).toBeDisabled();
    expect(secondButton).toBeDisabled();
    await user.click(firstButton);
    expect(api.callsTo("POST", QUOTA_FETCH)).toHaveLength(2);
    act(() => { finishRead?.(); });
    await waitFor(() => { expect(firstButton).toBeEnabled(); });
  });

  it("keeps fresh quota visible without a conflict assertion when routing reload fails", async () => {
    let reads = 0;
    mockApi(
      route("GET", AUTH_FILES, () => {
        reads += 1;
        return reads === 1 ? { json: credentialList([
          credential({ supports_quota: true, cooldowns: [cooldown("quota", 300)] }),
        ]) } : { status: 500, json: { error: "routing reload failed" } };
      }),
      route("POST", QUOTA_FETCH, { json: available }),
    );
    const { user } = renderApp("/credentials");
    const item = await screen.findByRole("article", { name: "ada@example.com" });
    await user.click(within(item).getByRole("button", { name: "Check quota of ada@example.com" }));
    const dialog = screen.getByRole("dialog", { name: "Quota of ada@example.com" });
    expect(await within(dialog).findByText("Routing state could not be reloaded")).toBeVisible();
    expect(within(dialog).getByText(/100% left/)).toBeVisible();
    expect(within(dialog).getByRole("status")).toHaveTextContent("Routing state could not be reloaded");
    expect(dialog).toHaveTextContent("The cooldown shown on the page may be out of date");
    expect(screen.queryByText("Proxy quota cooldown still cached")).toBeNull();
    expect(reads).toBeGreaterThan(1);
  });

  it("matches fresh routing state by credential id, not row position", async () => {
    let reads = 0;
    const target = credential({ supports_quota: true, cooldowns: [cooldown("quota", 300)] });
    const other = credential({ id: "other", name: "other.json", account: "other@example.com" });
    mockApi(
      route("GET", AUTH_FILES, () => {
        reads += 1;
        return { json: credentialList(reads === 1 ? [target, other] : [other, target]) };
      }),
      route("POST", QUOTA_FETCH, { json: available }),
    );
    const { user } = renderApp("/credentials");
    const item = await screen.findByRole("article", { name: "ada@example.com" });
    await user.click(within(item).getByRole("button", { name: "Check quota of ada@example.com" }));
    expect(await screen.findByText("Proxy quota cooldown still cached")).toBeVisible();
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
