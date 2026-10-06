import { lazy } from "react";
import { Navigate, Outlet, useLocation, type RouteObject } from "react-router";

import { AppShell } from "../layout/AppShell";
import { AboutPage } from "../pages/AboutPage";
import { NotFoundPage } from "../pages/NotFoundPage";
import { OverviewPage } from "../pages/OverviewPage";
import { SignInPage } from "../pages/SignInPage";
import { useSession } from "../session/session";

// The heavier pages load when first visited: the chart library is most of
// the app's code, and the sign-in page needs none of it.
const UsagePage = lazy(() =>
  import("../pages/usage/UsagePage").then((module) => ({ default: module.UsagePage })),
);
const LedgerPage = lazy(() =>
  import("../pages/usage/LedgerPage").then((module) => ({ default: module.LedgerPage })),
);
const LogsPage = lazy(() =>
  import("../pages/logs/LogsPage").then((module) => ({ default: module.LogsPage })),
);
const LogViewerPage = lazy(() =>
  import("../pages/logs/LogViewerPage").then((module) => ({ default: module.LogViewerPage })),
);

/** Where a signed-out visit was headed, kept across the sign-in. */
export interface ReturnTo {
  pathname: string;
  search: string;
}

/** Renders its routes while signed in; else sends the visit to sign in. */
function RequireSession() {
  const { key } = useSession();
  const location = useLocation();
  if (key === null) {
    const from: ReturnTo = { pathname: location.pathname, search: location.search };
    return <Navigate to="/signin" replace state={{ from }} />;
  }
  return <Outlet />;
}

export const routes: RouteObject[] = [
  { path: "/signin", element: <SignInPage /> },
  {
    element: <RequireSession />,
    children: [
      {
        element: <AppShell />,
        children: [
          { index: true, element: <OverviewPage /> },
          { path: "usage", element: <UsagePage /> },
          { path: "usage/ledger", element: <LedgerPage /> },
          { path: "logs", element: <LogsPage /> },
          { path: "logs/:name", element: <LogViewerPage /> },
          { path: "about", element: <AboutPage /> },
          { path: "*", element: <NotFoundPage /> },
        ],
      },
    ],
  },
];
