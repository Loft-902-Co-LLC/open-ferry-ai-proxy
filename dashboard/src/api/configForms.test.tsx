import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { focusManager, QueryClientProvider } from "@tanstack/react-query";
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
