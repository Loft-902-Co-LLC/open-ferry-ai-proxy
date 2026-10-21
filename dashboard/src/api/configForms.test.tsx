import { act, render, renderHook, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { focusManager, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { afterEach, expect, it } from "vitest";

import { createQueryClient } from "../app/QueryProvider";
import { SessionProvider } from "../session/session";
import { mockApi, route } from "../test/mockApi";
import { storeKey } from "./keyStorage";
import { useConfigEditor, useConfigQuery } from "./hooks";
import { CONFIG, MANAGEMENT, V8_CONFIG } from "./management";

afterEach(() => { focusManager.setFocused(undefined); });

function Form({ name }: { name: string }) {
  const editor = useConfigEditor();
  const query = useConfigQuery<{ "request-retry": number }>(editor, CONFIG, { staleTime: 0 });
  return <section aria-label={name}>
    <output aria-label={`${name} retries`}>{String(query.data?.["request-retry"] ?? "loading")}</output>
    {query.error !== null && <p role="alert">{query.error.message}</p>}
    <button onClick={() => {
      void editor.call(`${MANAGEMENT}/request-retry`, { method: "PUT", json: { value: 2 } });
    }}>Save {name}</button>
  </section>;
}

it("refetches clean sibling forms after a save and on window focus without conflicts", async () => {
  let retry = 1;
  let version = 1;
  const api = mockApi(
    route("GET", V8_CONFIG, () => ({
      json: { routing: { retry: { "request-retry": retry } } }, headers: { ETag: `"${String(version)}"` },
    })),
    route("PUT", `${V8_CONFIG}/routing/retry/request-retry`, (request) => {
      retry = Number(request.json()); version++;
      return { json: {}, headers: { ETag: `"${String(version)}"` } };
    }),
  );
  storeKey("unit-config-forms");
  render(<SessionProvider><QueryClientProvider client={createQueryClient(0)}>
    <Form name="first" /><Form name="second" />
  </QueryClientProvider></SessionProvider>);
  await waitFor(() => { expect(screen.getByLabelText("second retries")).toHaveTextContent("1"); });
  act(() => { focusManager.setFocused(false); });
  await userEvent.click(screen.getByRole("button", { name: "Save first" }));
  await waitFor(() => { expect(screen.getByLabelText("second retries")).toHaveTextContent("2"); });
  const reads = api.callsTo("GET", V8_CONFIG).length;
  act(() => { focusManager.setFocused(true); });
  await waitFor(() => { expect(api.callsTo("GET", V8_CONFIG).length).toBeGreaterThan(reads); });
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  expect(screen.getByLabelText("second retries")).toHaveTextContent("2");
});


it.each(["credential toggle", "provider removal"])("retries a %s after 412 and Reload without opting into Settings drafts", async (operation) => {
  let version = 1;
  let groups = [{ keys: [{ "api-key": "unit-provider", auth_index: "7", "excluded-models": [] as string[] }] }];
  const target = operation === "credential toggle" ? V8_CONFIG : `${V8_CONFIG}/api-keys/gemini`;
  const method = operation === "credential toggle" ? "PATCH" : "PUT";
  const api = mockApi(
    route("GET", V8_CONFIG, () => ({ json: { "api-keys": { gemini: groups } }, headers: { ETag: `"${String(version)}"` } })),
    route("GET", `${MANAGEMENT}/gemini-api-key`, { json: { "gemini-api-key": [{ "api-key": "unit-provider", "auth-index": "7" }] } }),
    route(method, target, (request) => {
      if (version === 1) { version++; return { status: 412, json: { error: "precondition_failed" } }; }
      if (request.headers.get("if-match") !== `"${String(version)}"`) return { status: 412 };
      groups = operation === "credential toggle"
        ? (request.json() as { "api-keys": { gemini: typeof groups } })["api-keys"].gemini
        : request.json() as typeof groups;
      version++;
      return { json: {}, headers: { ETag: `"${String(version)}"` } };
    }),
  );
  storeKey("unit-config-reload");
  const { result } = renderHook(() => useConfigEditor(), { wrapper: ({ children }: { children: ReactNode }) =>
    <SessionProvider><QueryClientProvider client={createQueryClient(0)}>{children}</QueryClientProvider></SessionProvider>,
  });
  const write = () => operation === "credential toggle"
    ? result.current.toggleCredential({ name: "unit-credential", auth_index: "7" }, true)
    : result.current.call(`${MANAGEMENT}/gemini-api-key`, { method: "DELETE", query: { index: 0 }, configTarget: { key: "unit-provider" } });
  await expect(write()).rejects.toMatchObject({ status: 412 });
  await act(async () => { await result.current.reload(); });
  await expect(write()).resolves.toBeDefined();
  expect(api.callsTo(method, target).map((call) => call.headers.get("if-match"))).toEqual(['"1"', '"2"']);
  if (operation === "credential toggle") expect(groups[0]?.keys[0]?.["excluded-models"]).toEqual(["*"]);
  else expect(groups).toEqual([]);
});
