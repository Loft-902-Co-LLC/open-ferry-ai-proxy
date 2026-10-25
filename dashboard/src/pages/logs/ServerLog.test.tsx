import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { storeKey } from "../../api/keyStorage";
import { SERVER_LOGS } from "../../api/management";
import { QueryProvider } from "../../app/QueryProvider";
import { SessionProvider } from "../../session/session";
import { serverLogPage } from "../../test/fixtures";
import { mockApi, route } from "../../test/mockApi";
import { TEST_KEY } from "../../test/renderApp";
import { ServerLog } from "./ServerLog";

function openServerLog() {
  storeKey(TEST_KEY);
  render(
    <SessionProvider>
      <QueryProvider retryDelay={0}>
        <MemoryRouter>
          <ServerLog />
        </MemoryRouter>
      </QueryProvider>
    </SessionProvider>,
  );
  return userEvent.setup();
}

describe("recovering a server log read", () => {
  beforeEach(() => {
    // Polling uses the fake clock; query retries and UI waits settle normally.
    vi.useFakeTimers({ toFake: ["Date", "setInterval", "clearInterval"] });
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it.each([true, false])("retries with the cached cursor when Follow is %s", async (follow) => {
    let reads = 0;
    let unreachable = true;
    const api = mockApi(
      route("GET", SERVER_LOGS, () => {
        reads += 1;
        if (reads === 1) {
          return { json: serverLogPage(["first line", "second line"]) };
        }
        if (unreachable) {
          throw new TypeError("Failed to fetch");
        }
        return reads === 5
          ? { json: serverLogPage(["third line"], { "next-cursor": "cursor-2" }) }
          : { json: serverLogPage(["fourth line"], { "next-cursor": "cursor-3" }) };
      }),
    );
    const user = openServerLog();
    const log = await screen.findByRole("log", { name: "Server log lines" });
    expect(Array.from(log.children, (line) => line.textContent)).toEqual(["first line", "second line"]);
    expect(api.callsTo("GET", SERVER_LOGS)[0]?.url.searchParams.get("cursor")).toBeNull();

    if (follow) {
      await act(() => vi.advanceTimersByTimeAsync(3000));
    } else {
      await user.click(screen.getByRole("checkbox", { name: "Follow" }));
      await user.click(screen.getByRole("button", { name: "Read new lines" }));
    }
    const notice = await screen.findByRole("alert");
    expect(notice).toHaveTextContent("The server didn't answer");
    expect(notice).toHaveTextContent(follow ? "Following stopped." : "Reading stopped.");
    expect(reads).toBe(4);
    expect(Array.from(log.children, (line) => line.textContent)).toEqual(["first line", "second line"]);
    expect(screen.getByRole("checkbox", { name: "Follow" })).toHaveProperty("checked", follow);
    await act(() => vi.advanceTimersByTimeAsync(30_000));
    expect(reads).toBe(4);
    for (const call of api.callsTo("GET", SERVER_LOGS).slice(1)) {
      expect(call.url.searchParams.get("cursor")).toBe("cursor-1");
    }

    unreachable = false;
    await user.click(within(notice).getByRole("button", { name: "Try again" }));
    await waitFor(() => {
      expect(log).toHaveTextContent("third line");
    });
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(Array.from(log.children, (line) => line.textContent)).toEqual([
      "first line", "second line", "third line",
    ]);
    expect(api.callsTo("GET", SERVER_LOGS)[4]?.url.searchParams.get("cursor")).toBe("cursor-1");

    await act(() => vi.advanceTimersByTimeAsync(3000));
    if (!follow) {
      expect(reads).toBe(5);
      await user.click(screen.getByRole("button", { name: "Read new lines" }));
    }
    await waitFor(() => {
      expect(log).toHaveTextContent("fourth line");
    });
    expect(reads).toBe(6);
    expect(api.callsTo("GET", SERVER_LOGS)[5]?.url.searchParams.get("cursor")).toBe("cursor-2");
    expect(Array.from(log.children, (line) => line.textContent)).toEqual([
      "first line", "second line", "third line", "fourth line",
    ]);
    expect(api.unhandled).toEqual([]);
  });

  it("stops after logging to a file is disabled, including an explicit retry", async () => {
    let reads = 0;
    const api = mockApi(
      route("GET", SERVER_LOGS, () => {
        reads += 1;
        return reads === 1
          ? { json: serverLogPage(["retained line"]) }
          : { status: 400, json: { error: "logging to file disabled" } };
      }),
    );
    const user = openServerLog();
    const log = await screen.findByRole("log", { name: "Server log lines" });
    await act(() => vi.advanceTimersByTimeAsync(3000));
    const notice = await screen.findByRole("alert");
    expect(reads).toBe(2);
    await act(() => vi.advanceTimersByTimeAsync(30_000));
    expect(reads).toBe(2);

    await user.click(within(notice).getByRole("button", { name: "Try again" }));
    await waitFor(() => {
      expect(reads).toBe(3);
      expect(within(notice).getByRole("button", { name: "Try again" })).toBeEnabled();
    });
    await act(() => vi.advanceTimersByTimeAsync(30_000));
    expect(reads).toBe(3);
    expect(log).toHaveTextContent("retained line");
    expect(api.callsTo("GET", SERVER_LOGS).at(-1)?.url.searchParams.get("cursor")).toBe("cursor-1");
    expect(api.unhandled).toEqual([]);
  });
});
