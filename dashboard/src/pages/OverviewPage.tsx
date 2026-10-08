import { useLocation } from "react-router";

import { asksForSafeModeSetup } from "../app/safeMode";
import { PageHeader } from "../components/PageHeader";
import { Loading } from "../components/QueryState";
import { ClientSetupCard } from "./overview/ClientSetupCard";
import { useOverviewLayout } from "./overview/overviewLayout";
import { ProvidersCard } from "./overview/ProvidersCard";
import { TodayCard } from "./overview/TodayCard";

/**
 * The first page. A proxy nothing uses yet gets its setup, first and open.
 * Once a client has connected, the page leads with how the proxy is doing:
 * today's calls, then the accounts, with the client setup closed below.
 * Safe mode always gets the setup, and so does a link from CLIProxyAPI's
 * safe-mode page, which opens it whatever the layout.
 */
export function OverviewPage() {
  const location = useLocation();
  const focusKeys = asksForSafeModeSetup(location.search);
  const layout = useOverviewLayout();

  let description;
  let content;
  if (layout === null) {
    content = <Loading>Reading the proxy&apos;s state…</Loading>;
  } else if (layout === "health") {
    description = "How the proxy is doing today.";
    content = (
      <>
        <TodayCard />
        <ProvidersCard />
        <ClientSetupCard collapsible focusKeys={focusKeys} />
      </>
    );
  } else {
    description = "Connect providers and clients to this proxy.";
    content = (
      <>
        <ProvidersCard />
        <ClientSetupCard focusKeys={focusKeys} />
      </>
    );
  }

  return (
    <>
      <PageHeader title="Overview" description={description} />
      <div className="space-y-4">{content}</div>
    </>
  );
}
