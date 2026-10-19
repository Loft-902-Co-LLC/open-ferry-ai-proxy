import { act, renderHook } from "@testing-library/react";
import type { ReactNode } from "react";
import { afterEach, expect, it, vi } from "vitest";

import { QueryProvider } from "../app/QueryProvider";
import { SessionProvider } from "../session/session";
import { supervisorStatus, updateStatus } from "../test/fixtures";
import { mockApi, route } from "../test/mockApi";
import { TEST_KEY } from "../test/renderApp";
import { storeKey } from "./keyStorage";
import { UPDATE } from "./update";
import { STATUS_POLL_TIMEOUT_MS, useUpdateStatus } from "./updateStatus";

function observeStatus() {
  storeKey(TEST_KEY);
  return renderHook(() => useUpdateStatus(), { wrapper: ({ children }: { children: ReactNode }) =>
    <SessionProvider><QueryProvider retryDelay={0}>{children}</QueryProvider></SessionProvider>,
  });
}

function fakePollClock() {
  vi.useFakeTimers();
  // The DOM signal uses its own clock; keep it on the test's clock as well.
  vi.spyOn(AbortSignal, "timeout").mockImplementation((ms) => {
    const controller = new AbortController();
    setTimeout(() => { controller.abort(new DOMException("status poll timed out", "TimeoutError")); }, ms);
    return controller.signal;
  });
}

afterEach(() => { vi.useRealTimers(); });

it("times out hanging polls individually and reaches the failed-poll limit", async () => {
  fakePollClock();
  mockApi(route("GET", UPDATE, { json: updateStatus({ supervisor: supervisorStatus({ phase: "handing-over" }) }) }));
  const firstFetch = globalThis.fetch;
  let reads = 0;
  vi.stubGlobal("fetch", (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
    if (++reads === 1) return firstFetch(input, init);
    return new Promise((_resolve, reject) => {
      const signal = init?.signal;
      signal?.addEventListener("abort", () => { reject(signal.reason); }, { once: true });
    });
  });
  const { result } = observeStatus();
  await act(() => vi.advanceTimersByTimeAsync(0));
  await act(() => vi.advanceTimersByTimeAsync(2_000 + STATUS_POLL_TIMEOUT_MS - 1));
  expect(reads).toBe(2);
  expect(result.current.isError).toBe(false);
  await act(() => vi.advanceTimersByTimeAsync(1));
  expect(result.current.isError).toBe(true);
  expect(result.current.hidePollError).toBe(true);
  await act(() => vi.advanceTimersByTimeAsync(4 * (2_000 + STATUS_POLL_TIMEOUT_MS)));
  expect(reads).toBe(6);
  expect(result.current.hidePollError).toBe(false);
  await act(() => vi.advanceTimersByTimeAsync(2_001));
  expect(reads).toBe(6);
});
