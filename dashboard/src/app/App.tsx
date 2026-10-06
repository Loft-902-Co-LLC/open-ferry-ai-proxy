import { useState } from "react";
import { RouterProvider, createBrowserRouter } from "react-router";

import { SessionProvider } from "../session/session";
import { QueryProvider } from "./QueryProvider";
import { routes } from "./routes";

/** The path open-ferry serves the app under, without its trailing slash. */
export const BASENAME = import.meta.env.BASE_URL.replace(/\/$/, "");

export function App() {
  const [router] = useState(() => createBrowserRouter(routes, { basename: BASENAME }));
  return (
    <SessionProvider>
      <QueryProvider>
        <RouterProvider router={router} />
      </QueryProvider>
    </SessionProvider>
  );
}
