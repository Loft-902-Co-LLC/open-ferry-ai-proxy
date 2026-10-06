import { useLocation } from "react-router";

import { asksForSafeModeSetup } from "../app/safeMode";
import { Alert } from "../components/Alert";
import { Card } from "../components/Card";
import { PageHeader } from "../components/PageHeader";

export function OverviewPage() {
  const location = useLocation();
  return (
    <>
      <PageHeader title="Overview" description="The state of this proxy at a glance." />
      <div className="space-y-4">
        {asksForSafeModeSetup(location.search) && (
          <Alert tone="warn" title="The proxy is in safe mode">
            <p>
              Its client API keys are still CLIProxyAPI&apos;s examples, so it refuses proxy
              requests until they are replaced.
            </p>
          </Alert>
        )}
        <Card title="Signed in">
          <p>You are signed in to this server&apos;s management API.</p>
        </Card>
      </div>
    </>
  );
}
