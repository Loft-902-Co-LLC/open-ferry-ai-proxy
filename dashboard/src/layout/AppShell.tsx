import { LogOut } from "lucide-react";
import { Suspense } from "react";
import { Link, NavLink, Outlet } from "react-router";

import { Brand } from "../components/Brand";
import { Button } from "../components/Button";
import { Loading } from "../components/QueryState";
import { useSession } from "../session/session";

/** The pages in the main navigation, in order. */
export const NAV_ITEMS: readonly { to: string; label: string }[] = [
  { to: "/", label: "Overview" },
  { to: "/usage", label: "Usage" },
  { to: "/about", label: "About" },
];

/** The frame of every signed-in page: header, navigation and content. */
export function AppShell() {
  const { signOut } = useSession();
  return (
    <div className="flex min-h-screen flex-col">
      <a
        href="#main"
        className="sr-only rounded-md bg-surface px-3 py-2 focus:not-sr-only focus:absolute focus:top-2 focus:left-2 focus:z-10"
      >
        Skip to content
      </a>
      <header className="border-b border-line bg-surface">
        <div className="mx-auto flex max-w-7xl flex-wrap items-center gap-x-6 gap-y-1 px-4 py-2">
          <Link to="/" className="text-fg hover:no-underline" aria-label="open-ferry overview">
            <Brand />
          </Link>
          <nav aria-label="Main" className="order-last -mx-1 w-full overflow-x-auto sm:order-none sm:w-auto sm:flex-1">
            <ul className="flex gap-1">
              {NAV_ITEMS.map((item) => (
                <li key={item.to}>
                  <NavLink
                    to={item.to}
                    end={item.to === "/"}
                    className="block rounded-md px-2.5 py-1.5 font-medium whitespace-nowrap text-muted hover:bg-raised hover:text-fg hover:no-underline aria-[current=page]:bg-raised aria-[current=page]:text-fg"
                  >
                    {item.label}
                  </NavLink>
                </li>
              ))}
            </ul>
          </nav>
          <Button
            variant="ghost"
            size="sm"
            className="ml-auto sm:ml-0"
            onClick={() => {
              signOut();
            }}
          >
            <LogOut aria-hidden="true" className="size-4" />
            Sign out
          </Button>
        </div>
      </header>
      <main id="main" tabIndex={-1} className="mx-auto w-full max-w-7xl flex-1 px-4 py-6 focus:outline-none">
        <Suspense fallback={<Loading>Loading the page…</Loading>}>
          <Outlet />
        </Suspense>
      </main>
    </div>
  );
}
