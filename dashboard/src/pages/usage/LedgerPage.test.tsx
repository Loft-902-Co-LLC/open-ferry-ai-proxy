import { screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { USAGE_LEDGER, USAGE_PRICES, USAGE_RECORDS } from "../../api/dashboard";
import { ledger, prices } from "../../test/fixtures";
import { loadFirst } from "../../test/loadFirst";
import { mockApi, route, type MockRoute } from "../../test/mockApi";
import { renderApp } from "../../test/renderApp";

function ledgerServer(...overrides: MockRoute[]) {
  const api = mockApi(
    route("GET", USAGE_LEDGER, { json: ledger() }),
    route("GET", USAGE_PRICES, { json: prices() }),
  );
  api.use(...overrides);
  return api;
}

loadFirst(() => import("./LedgerPage"));

describe("the Ledger and prices page", () => {
  it("describes the ledger", async () => {
    ledgerServer();
    renderApp("/usage/ledger");
    expect(await screen.findByRole("heading", { name: "Ledger and prices", level: 1 })).toBeVisible();
    expect(await screen.findByText("/srv/open-ferry/logs/open-ferry-usage.sqlite3")).toBeVisible();
    expect(screen.getByText("48,213")).toBeVisible();
    expect(screen.getByText("18 MiB")).toBeVisible();
  });

  it("saves only the settings that changed", async () => {
    const api = ledgerServer(
      route("PATCH", USAGE_LEDGER, (request) => ({
        json: ledger(request.json() as Partial<ReturnType<typeof ledger>>),
      })),
    );
    const { user } = renderApp("/usage/ledger");
    const days = await screen.findByRole("textbox", { name: "Keep calls for (days)" });
    expect(days).toHaveValue("90");
    expect(screen.getByRole("button", { name: "Save" })).toBeDisabled();

    await user.clear(days);
    await user.type(days, "30");
    await user.click(screen.getByRole("button", { name: "Save" }));

    expect(await screen.findByText("Saved.")).toBeVisible();
    expect(api.callsTo("PATCH", USAGE_LEDGER)[0]?.json()).toEqual({ retention_days: 30 });
    expect(days).toHaveValue("30");
  });

  it("checks the settings as the server does", async () => {
    const api = ledgerServer();
    const { user } = renderApp("/usage/ledger");
    const rows = await screen.findByRole("textbox", { name: "Keep at most (calls)" });
    await user.clear(rows);
    await user.type(rows, "500");
    const currency = screen.getByRole("textbox", { name: "Currency" });
    await user.clear(currency);
    await user.type(currency, "US$");
    await user.click(screen.getByRole("button", { name: "Save" }));

    expect(
      await screen.findByText("Keep at most is a whole number from 10,000 to 10,000,000."),
    ).toBeVisible();
    expect(screen.getByText("The currency is 1 to 8 letters, such as USD.")).toBeVisible();
    expect(rows).toHaveAttribute("aria-invalid", "true");
    expect(api.callsTo("PATCH", USAGE_LEDGER)).toEqual([]);
  });

  it("lists prices and sets one for a model without", async () => {
    let saved = false;
    const api = ledgerServer(
      route("GET", USAGE_PRICES, () => ({
        json: saved
          ? prices({
              prices: [
                ...prices().prices,
                {
                  model: "claude-sonnet-4-5",
                  input: 3,
                  cache_read: null,
                  cache_write: null,
                  output: 15,
                  updated: "2026-10-05T12:00:00.000Z",
                },
              ],
              unpriced_models: [],
            })
          : prices(),
      })),
      route("PUT", USAGE_PRICES, (request) => {
        saved = true;
        return { json: { ...(request.json() as object), updated: "2026-10-05T12:00:00.000Z" } };
      }),
    );
    const { user } = renderApp("/usage/ledger");
    const table = await screen.findByRole("table", { name: "Prices, in USD per million tokens" });
    expect(within(table).getByText("gpt-5.1-codex")).toBeVisible();
    expect(within(table).getByText("as input")).toBeVisible();

    await user.click(screen.getByRole("button", { name: "Set a price for claude-sonnet-4-5" }));
    const form = screen.getByRole("form", { name: "Set a price" });
    expect(within(form).getByRole("combobox", { name: "Model" })).toHaveValue("claude-sonnet-4-5");
    await user.type(within(form).getByRole("textbox", { name: "Input" }), "3");
    await user.type(within(form).getByRole("textbox", { name: "Output" }), "15");
    await user.click(within(form).getByRole("button", { name: "Save price" }));

    expect(await screen.findByText("Price saved. Costs, past ones too, now use it.")).toBeVisible();
    expect(api.callsTo("PUT", USAGE_PRICES)[0]?.json()).toEqual({
      model: "claude-sonnet-4-5",
      input: 3,
      cache_read: null,
      cache_write: null,
      output: 15,
    });
    expect(await within(table).findByText("claude-sonnet-4-5")).toBeVisible();
  });

  it("checks a price before sending it", async () => {
    const api = ledgerServer();
    const { user } = renderApp("/usage/ledger");
    const form = await screen.findByRole("form", { name: "Set a price" });
    await user.type(within(form).getByRole("textbox", { name: "Input" }), "-1");
    await user.click(within(form).getByRole("button", { name: "Save price" }));
    expect(await within(form).findByText("Enter the model, as it is sent upstream.")).toBeVisible();
    expect(within(form).getByText("The input price is a number from 0 to 1,000,000.")).toBeVisible();
    expect(within(form).getByText("The output price is a number from 0 to 1,000,000.")).toBeVisible();
    expect(api.callsTo("PUT", USAGE_PRICES)).toEqual([]);
  });

  it("changes and removes a price", async () => {
    const api = ledgerServer(
      route("DELETE", USAGE_PRICES, { json: { deleted: true } }),
    );
    const { user } = renderApp("/usage/ledger");
    await user.click(
      await screen.findByRole("button", { name: "Change the price of gpt-5.1-codex" }),
    );
    const form = screen.getByRole("form", { name: "Set a price" });
    expect(within(form).getByRole("textbox", { name: "Cache read" })).toHaveValue("0.125");
    expect(within(form).getByRole("textbox", { name: "Cache write" })).toHaveValue("");

    await user.click(screen.getByRole("button", { name: "Remove the price of gpt-5.1-codex" }));
    await waitFor(() => {
      expect(api.callsTo("DELETE", USAGE_PRICES)).toHaveLength(1);
    });
    expect(api.callsTo("DELETE", USAGE_PRICES)[0]?.url.searchParams.get("model")).toBe(
      "gpt-5.1-codex",
    );
  });

  it("deletes the recorded calls only once confirmed", async () => {
    const api = ledgerServer(route("DELETE", USAGE_RECORDS, { json: { deleted: 48_213 } }));
    const { user } = renderApp("/usage/ledger");

    await user.click(await screen.findByRole("button", { name: "Delete all recorded calls" }));
    let dialog = screen.getByRole("dialog", { name: "Delete every recorded call?" });
    expect(within(dialog).getByRole("button", { name: "Cancel" })).toHaveFocus();
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(api.callsTo("DELETE", USAGE_RECORDS)).toEqual([]);

    await user.click(screen.getByRole("button", { name: "Delete all recorded calls" }));
    dialog = screen.getByRole("dialog", { name: "Delete every recorded call?" });
    await user.click(within(dialog).getByRole("button", { name: "Delete 48,213 calls" }));
    expect(await screen.findByText("Deleted 48,213 calls.")).toBeVisible();
    expect(api.callsTo("DELETE", USAGE_RECORDS)).toHaveLength(1);
  });
});
