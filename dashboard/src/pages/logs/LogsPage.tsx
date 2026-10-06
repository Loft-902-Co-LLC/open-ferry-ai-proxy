import { useSearchParams } from "react-router";

import { PageHeader } from "../../components/PageHeader";
import { Tabs } from "../../components/Tabs";
import { RequestLogSearch } from "./RequestLogSearch";
import { ServerLog } from "./ServerLog";

const TABS = [
  { id: "requests", label: "Request logs" },
  { id: "server", label: "Server log" },
] as const;

/** The request and error logs, and the server's own log. */
export function LogsPage() {
  const [params, setParams] = useSearchParams();
  const tab = params.get("tab") === "server" ? "server" : "requests";
  return (
    <>
      <PageHeader
        title="Logs"
        description="What the server wrote about each request, and its own log."
      />
      <Tabs
        label="Logs"
        items={TABS}
        selected={tab}
        onSelect={(id) => {
          setParams((current) => {
            const next = new URLSearchParams(current);
            if (id === "server") {
              next.set("tab", "server");
            } else {
              next.delete("tab");
            }
            return next;
          });
        }}
      >
        {tab === "server" ? <ServerLog /> : <RequestLogSearch />}
      </Tabs>
    </>
  );
}
