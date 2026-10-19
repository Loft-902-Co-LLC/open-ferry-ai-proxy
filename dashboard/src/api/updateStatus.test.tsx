import { focusManager } from "@tanstack/react-query";
import { act, renderHook } from "@testing-library/react";
import type { ReactNode } from "react";
import { afterEach, expect, it, vi } from "vitest";

import { QueryProvider } from "../app/QueryProvider";
import { SessionProvider } from "../session/session";
import { supervisorStatus, updateStatus } from "../test/fixtures";
import { mockApi, route } from "../test/mockApi";
import { TEST_KEY } from "../test/renderApp";
import { isApiError } from "./client";
import { storeKey } from "./keyStorage";
import { UPDATE } from "./update";
import { MAX_POLL_ERROR_MS, STATUS_POLL_TIMEOUT_MS, useUpdateStatus } from "./updateStatus";

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
      signal?.addEventListener("abort", () => { reject(signal.reason as Error); }, { once: true });
    });
  });
  const { result } = observeStatus();
  await act(() => vi.advanceTimersByTimeAsync(0));
  await act(() => vi.advanceTimersByTimeAsync(2_000 + STATUS_POLL_TIMEOUT_MS - 1));
  expect(reads).toBe(2);
  expect(result.current.isError).toBe(false);
  await act(() => vi.advanceTimersByTimeAsync(1));
  expect(result.current.isError).toBe(true);
  expect(isApiError(result.current.error) && result.current.error.status === 0).toBe(true);
  expect(result.current.hidePollError).toBe(true);
  await act(() => vi.advanceTimersByTimeAsync(4 * (2_000 + STATUS_POLL_TIMEOUT_MS)));
  expect(reads).toBe(6);
  expect(result.current.hidePollError).toBe(false);
  await act(() => vi.advanceTimersByTimeAsync(2_001));
  expect(reads).toBe(6);
});


it("ignores an old deadline callback queued just as a successful read arrives", async () => {
  fakePollClock();
  const timers = vi.spyOn(globalThis, "setTimeout");
  let fail = false;
  let active = false;
  mockApi(route("GET", UPDATE, () => {
    if (fail) throw new TypeError("offline");
    return { json: updateStatus({ supervisor: supervisorStatus({ phase: active ? "handing-over" : "idle" }) }) };
  }));
  const { result } = observeStatus();
  await act(() => vi.advanceTimersByTimeAsync(0));
  fail = true;
  act(() => { void result.current.refetch(); });
  await act(() => vi.advanceTimersByTimeAsync(10));
  await act(() => vi.advanceTimersByTimeAsync(0));
  const callback = timers.mock.calls.find(([, delay]) => delay === MAX_POLL_ERROR_MS)?.[0] as (() => void) | undefined;
  expect(typeof callback).toBe("function");
  await act(() => vi.advanceTimersByTimeAsync(MAX_POLL_ERROR_MS - 1));
  fail = false;
  active = true;
  act(() => { void result.current.refetch(); });
  await act(() => vi.advanceTimersByTimeAsync(10));
  await act(() => vi.advanceTimersByTimeAsync(0));
  // Even a callback already queued before the success may still run.
  await act(async () => { if (typeof callback === "function") callback(); await Promise.resolve(); });
  expect(result.current.hidePollError).toBe(true);
  expect(result.current.isError).toBe(false);
  const read = result.current.lastRead;
  await act(() => vi.advanceTimersByTimeAsync(2_001));
  expect(result.current.lastRead).not.toBe(read);
});


it.each([false, true])("preserves global retries outside switching (switching=%s)", async (switching) => {
  fakePollClock();
  let fail = false;
  const api = mockApi(route("GET", UPDATE, () => {
    if (fail) throw new TypeError("offline");
    return { json: updateStatus({ supervisor: switching ? supervisorStatus({ phase: "watching" }) : null }) };
  }));
  const { result } = observeStatus();
  await act(() => vi.advanceTimersByTimeAsync(0));
  fail = true;
  act(() => { void result.current.refetch(); });
  await act(() => vi.advanceTimersByTimeAsync(10));
  expect(result.current.isError).toBe(true);
  expect(api.callsTo("GET", UPDATE)).toHaveLength(switching ? 2 : 4);
});

it("retries an initial status read before a switch is known", async () => {
  fakePollClock();
  let reads = 0;
  mockApi(route("GET", UPDATE, () => {
    if (++reads < 3) throw new TypeError("offline");
    return { json: updateStatus() };
  }));
  const { result } = observeStatus();
  await act(() => vi.advanceTimersByTimeAsync(10));
  expect(reads).toBe(3);
  expect(result.current.isSuccess).toBe(true);
});


it("reveals one failed poll at the deadline while the tab stays unfocused", async () => {
  fakePollClock();
  let reads = 0;
  mockApi(route("GET", UPDATE, () => {
    if (++reads > 1) throw new TypeError("offline");
    return { json: updateStatus({ supervisor: supervisorStatus({ phase: "handing-over" }) }) };
  }));
  const { result, unmount } = observeStatus();
  try {
    await act(() => vi.advanceTimersByTimeAsync(0));
    act(() => { void result.current.refetch(); });
    await act(() => vi.advanceTimersByTimeAsync(10));
    expect(reads).toBe(2);
    expect(result.current.isError).toBe(true);
    expect(result.current.hidePollError).toBe(true);
    focusManager.setFocused(false);
    await act(() => vi.advanceTimersByTimeAsync(MAX_POLL_ERROR_MS));
    expect(result.current.hidePollError).toBe(false);
    await act(() => vi.advanceTimersByTimeAsync(4_001));
    expect(reads).toBe(2);
  } finally {
    unmount();
    focusManager.setFocused(undefined);
  }
});


it("counts a checking poll once after its retry attempts finish", async () => {
  fakePollClock();
  let reads = 0;
  mockApi(route("GET", UPDATE, () => {
    if (++reads > 1) throw new TypeError("offline");
    return { json: updateStatus({ checking: true }) };
  }));
  const { result } = observeStatus();
  await act(() => vi.advanceTimersByTimeAsync(0));
  await act(() => vi.advanceTimersByTimeAsync(8_010));
  expect(reads).toBe(13); // Four polls with three attempts each, plus the initial read.
  expect(result.current.isError).toBe(true);
  await act(() => vi.advanceTimersByTimeAsync(2_010));
  expect(reads).toBe(16);
  await act(() => vi.advanceTimersByTimeAsync(4_010));
  expect(reads).toBe(16);
});


it.each([true, false])("observes a degraded supervisor after a switch within the poll bound (recovers=%s)", async (recovers) => {
  fakePollClock();
  let reads = 0;
  mockApi(route("GET", UPDATE, () => ({ json: updateStatus({ supervisor: supervisorStatus({
    phase: ++reads === 1 ? "handing-over" : recovers && reads >= 3 ? "idle" : "degraded",
    switch: null,
  }) }) })));
  const { result } = observeStatus();
  await act(() => vi.advanceTimersByTimeAsync(0));
  await act(() => vi.advanceTimersByTimeAsync(2_001));
  expect(result.current.recoveringSupervisor).toBe(true);
  await act(() => vi.advanceTimersByTimeAsync(10_001));
  expect(reads).toBe(recovers ? 3 : 6);
  expect(result.current.recoveringSupervisor).toBe(!recovers);
  const stopped = reads;
  await act(() => vi.advanceTimersByTimeAsync(4_001));
  expect(reads).toBe(stopped);
});

it("does not start observing an initially degraded supervisor", async () => {
  fakePollClock();
  const api = mockApi(route("GET", UPDATE, { json: updateStatus({ supervisor: supervisorStatus({ phase: "degraded" }) }) }));
  const { result } = observeStatus();
  await act(() => vi.advanceTimersByTimeAsync(10_001));
  expect(api.callsTo("GET", UPDATE)).toHaveLength(1);
  expect(result.current.recoveringSupervisor).toBe(false);
});


it.each(["timeout", "cancel"])("links both signals without AbortSignal.any (%s)", async (end) => {
  fakePollClock();
  vi.stubGlobal("AbortSignal", { timeout: AbortSignal.timeout });
  let combined: AbortSignal | undefined;
  vi.stubGlobal("fetch", (_input: RequestInfo | URL, init?: RequestInit) => new Promise<Response>((_resolve, reject) => {
    combined = init?.signal ?? undefined;
    combined?.addEventListener("abort", () => { reject(combined?.reason as Error); }, { once: true });
  }));
  const { result, unmount } = observeStatus();
  await act(() => vi.advanceTimersByTimeAsync(0));
  expect(combined?.aborted).toBe(false);
  if (end === "cancel") {
    unmount();
    expect(combined?.aborted).toBe(true);
    expect((combined?.reason as DOMException).name).toBe("AbortError");
  } else {
    await act(() => vi.advanceTimersByTimeAsync(STATUS_POLL_TIMEOUT_MS));
    expect(combined?.aborted).toBe(true);
    expect((combined?.reason as DOMException).name).toBe("TimeoutError");
    await act(() => vi.advanceTimersByTimeAsync(1));
    // An initial read follows the shared retry policy; its first failure is a status-0 ApiError.
    expect(isApiError(result.current.failureReason) && result.current.failureReason.status === 0).toBe(true);
  }
});
