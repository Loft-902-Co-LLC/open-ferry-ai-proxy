import { useSearchParams } from "react-router";

import { PageHeader } from "../../components/PageHeader";
import { Tabs } from "../../components/Tabs";
import { ConfigFileEditor } from "./ConfigFileEditor";
import { SettingsTab } from "./SettingsTab";

const TABS = [
  { id: "settings", label: "Settings" },
  { id: "file", label: "config.yaml" },
] as const;

/** The server's settings: the common ones as a form, and config.yaml itself. */
export function SettingsPage() {
  const [params, setParams] = useSearchParams();
  const tab = params.get("tab") === "file" ? "file" : "settings";
  return (
    <>
      <PageHeader
        title="Settings"
        description="How the server works, kept in its config.yaml. Changes wait until you review and save them."
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
