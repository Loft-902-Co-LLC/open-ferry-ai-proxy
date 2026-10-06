import { useLocation } from "react-router";

import { asksForSafeModeSetup } from "../app/safeMode";
import { PageHeader } from "../components/PageHeader";
import { ClientSetupCard } from "./overview/ClientSetupCard";

export function OverviewPage() {
  const location = useLocation();
  return (
    <>
      <PageHeader title="Overview" description="Connect clients to this proxy." />
      <div className="space-y-4">
        <ClientSetupCard focusKeys={asksForSafeModeSetup(location.search)} />
      </div>
    </>
  );
}
