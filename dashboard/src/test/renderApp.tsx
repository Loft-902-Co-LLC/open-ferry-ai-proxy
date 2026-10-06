import { render } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { RouterProvider, createMemoryRouter } from "react-router";

import { storeKey } from "../api/keyStorage";
import { QueryProvider } from "../app/QueryProvider";
import { routes } from "../app/routes";
import { SessionProvider } from "../session/session";

/** A management key for tests; not a real one. */
export const TEST_KEY = "test-management-key-0001";

export interface RenderAppOptions {
  /** Sign in with this key first; null to start signed out. */
  key?: string | null;
}

/** Renders the whole app at `path`, as it runs in a browser. */
export function renderApp(path = "/", { key = TEST_KEY }: RenderAppOptions = {}) {
  if (key !== null) {
    storeKey(key);
  }
  const router = createMemoryRouter(routes, { initialEntries: [path] });
  const user = userEvent.setup();
  const view = render(
    <SessionProvider>
      <QueryProvider retryDelay={0}>
        <RouterProvider router={router} />
      </QueryProvider>
    </SessionProvider>,
  );
  return { ...view, router, user };
}
