import { Navigate, Outlet, useLocation, type RouteObject } from "react-router";

import { AppShell } from "../layout/AppShell";
import { AboutPage } from "../pages/AboutPage";
import { NotFoundPage } from "../pages/NotFoundPage";
import { OverviewPage } from "../pages/OverviewPage";
import { SignInPage } from "../pages/SignInPage";
import { useSession } from "../session/session";

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
          { path: "about", element: <AboutPage /> },
          { path: "*", element: <NotFoundPage /> },
        ],
      },
    ],
  },
];
