import { useLocation } from "react-router";

import { asksForSafeModeSetup } from "../app/safeMode";
import { PageHeader } from "../components/PageHeader";
import { ClientSetupCard } from "./overview/ClientSetupCard";
import { ProvidersCard } from "./overview/ProvidersCard";

export function OverviewPage() {
  const location = useLocation();
  return (
    <>
      <PageHeader
        title="Overview"
        description="Connect providers and clients to this proxy."
      />
      <div className="space-y-4">
        <ProvidersCard />
        <ClientSetupCard focusKeys={asksForSafeModeSetup(location.search)} />
      </div>
    </>
  );
}
