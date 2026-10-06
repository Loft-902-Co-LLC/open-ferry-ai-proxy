import { useSearchParams } from "react-router";

import { isUnsupportedRoute } from "../../api/client";
import { useApiQuery } from "../../api/hooks";
import { CONFIG } from "../../api/management";
import { Alert } from "../../components/Alert";
import { PageHeader } from "../../components/PageHeader";
import { QueryState } from "../../components/QueryState";
import { Tabs } from "../../components/Tabs";
import { ClientKeysCard } from "./ClientKeysCard";
import { ConfigFileEditor } from "./ConfigFileEditor";
import { SettingsForm } from "./SettingsForm";

const TABS = [
  { id: "settings", label: "Settings" },
  { id: "file", label: "config.yaml" },
] as const;

function SettingsTab() {
  const config = useApiQuery<unknown>(CONFIG);
  return (
    <div className="space-y-4">
      <ClientKeysCard />
      {isUnsupportedRoute(config.error) ? (
        <Alert tone="info" title="Settings can't be changed here">
          <p>
            This server doesn&apos;t serve its settings to the dashboard. Change them in config.yaml
            itself: the server picks the change up when it reloads the file.
          </p>
        </Alert>
      ) : (
        <QueryState query={config} loading="Reading the settings…">
          {(answer) => <SettingsForm config={answer} />}
        </QueryState>
      )}
    </div>
  );
}

/** The server's settings: the common ones as a form, and config.yaml itself. */
export function SettingsPage() {
  const [params, setParams] = useSearchParams();
  const tab = params.get("tab") === "file" ? "file" : "settings";
  return (
    <>
      <PageHeader
        title="Settings"
        description="How the server works, saved in its config.yaml. Changes take effect when saved."
      />
      <Tabs
        label="Settings"
        items={TABS}
        selected={tab}
        onSelect={(id) => {
          setParams((current) => {
            const next = new URLSearchParams(current);
            if (id === "file") {
              next.set("tab", "file");
            } else {
              next.delete("tab");
            }
            return next;
          });
        }}
      >
        {/* Both stay mounted, so switching tabs keeps unsaved edits. */}
        <div hidden={tab !== "settings"}>
          <SettingsTab />
        </div>
        <div hidden={tab !== "file"}>
          <ConfigFileEditor />
        </div>
      </Tabs>
    </>
  );
}
