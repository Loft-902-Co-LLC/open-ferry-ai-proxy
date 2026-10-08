import { useMutation, useQueryClient } from "@tanstack/react-query";
import { RefreshCw, RotateCw } from "lucide-react";
import type { ReactNode } from "react";
import { Link } from "react-router";

import { callProblem } from "../../api/access";
import { apiQueryKey, useApiCall } from "../../api/hooks";
import {
  UPDATE,
  UPDATE_CHECK,
  isUpdatesOff,
  isUpdatesUnavailable,
  type CheckStarted,
  type UpdateModeSource,
  type UpdateResult,
  type UpdateStatus,
} from "../../api/update";
import { useUpdateStatus } from "../../api/updateStatus";
import { Alert } from "../../components/Alert";
import { Badge, type BadgeTone } from "../../components/Badge";
import { Button } from "../../components/Button";
import { Card } from "../../components/Card";
import { Code } from "../../components/Code";
import { ProblemNotice } from "../../components/ProblemNotice";
import { Loading } from "../../components/QueryState";
import { Spinner } from "../../components/Spinner";
import { formatDateTime, formatSeconds } from "../../lib/format";

const UPDATES_DOC_URL =
  "https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/blob/main/docs/updates.md";

const UPDATES: Record<UpdateStatus["updates"], { label: string; tone: BadgeTone }> = {
  on: { label: "On", tone: "ok" },
  "notify-only": { label: "Notify only", tone: "info" },
  off: { label: "Off", tone: "neutral" },
};

const SOURCES: Record<UpdateModeSource, ReactNode> = {
  default: "by default",
  config: "set in config.yaml",
  environment: (
    <>
      set by <Code>OPEN_FERRY_SELF_UPDATE</Code> in the server&apos;s environment
    </>
  ),
};

/** What the last check found, in words. */
const RESULTS: Record<UpdateResult, string> = {
  "up-to-date": "up to date",
  "update-available": "a newer release is out",
  "cannot-update": "a newer release is out, which this install doesn't install itself",
  staged: "a newer release is ready to install",
  skipped: "the newer release failed here before, so it was skipped",
  error: "the check failed",
};

function Fact({ term, children }: { term: string; children: ReactNode }) {
  return (
    <div className="flex flex-wrap gap-x-3">
      <dt className="w-36 shrink-0 text-muted">{term}</dt>
      <dd className="min-w-0">{children}</dd>
    </div>
  );
}

function Version({ children }: { children: string }) {
  return <span className="font-mono break-all">{children}</span>;
}

/** What the server's state calls for: a restart, an install, a fix. */
function Notices({ status }: { status: UpdateStatus }) {
  const staged =
    status.staged_version !== null && status.staged_version !== status.installed_version
      ? status.staged_version
      : null;
  return (
    <>
      {!status.trusts_release_key && (
        <Alert tone="warn" title="This build trusts no release key">
          <p>
            It makes no update request and never updates itself. Release builds carry the key; a
            build from source before the first key was added has none.
          </p>
        </Alert>
      )}
      {status.restart_needed && (
        <Alert tone="info" title={`Restart the server to run ${status.installed_version}`}>
          <p>
            {status.installed_version} is installed, and the server runs {status.running_version}{" "}
            until it restarts.{" "}
            <a href={`${UPDATES_DOC_URL}#installing-an-update`} rel="noreferrer" target="_blank">
              How to restart it
            </a>
            .
          </p>
        </Alert>
      )}
      {staged !== null && (
        <Alert tone="info" title={`open-ferry ${staged} is ready to install`}>
          <p>
            It was downloaded and its signature checked. To install it, run{" "}
            <Code>open-ferry update</Code> on the server&apos;s computer, then restart the
            server.
          </p>
        </Alert>
      )}
      {staged === null && status.update_available && status.latest_version !== null && (
        <Alert tone="info" title={`open-ferry ${status.latest_version} is out`}>
          {!status.can_update_itself ? (
            <p>
              {status.why_not ?? "This install doesn't update itself."} Update it the way you
              installed it.
            </p>
          ) : (
            <p>
              {status.mode === "notify" && "Updates are notify-only, so nothing was downloaded. "}
              To install it, run <Code>open-ferry update</Code> on the server&apos;s
              computer.
            </p>
          )}
        </Alert>
      )}
      {status.last_result === "error" && status.last_error !== null && (
        <Alert tone="danger" title="The last check failed">
          <p className="break-words">{status.last_error}</p>
        </Alert>
      )}
      {status.notes.length > 0 && (
        <Alert tone="warn" title="Update settings the server couldn't use">
          <ul className="list-disc space-y-1 pl-5">
            {status.notes.map((note) => (
              <li key={note}>{note}</li>
            ))}
          </ul>
        </Alert>
      )}
    </>
  );
}

function Facts({ status }: { status: UpdateStatus }) {
  const updates = UPDATES[status.updates];
  const skipped = [
    ...status.failed_versions,
    ...(status.rolled_back_version === null ? [] : [status.rolled_back_version]),
  ];
  return (
    <dl className="space-y-1.5">
      <Fact term="Updates">
        <Badge tone={updates.tone}>{updates.label}</Badge> {SOURCES[status.mode_source]}
      </Fact>
      <Fact term="Running">
        <Version>{status.running_version}</Version>
      </Fact>
      {status.installed_version !== status.running_version && (
        <Fact term="Installed">
          <Version>{status.installed_version}</Version>
        </Fact>
      )}
      <Fact term="Latest release">
        {status.latest_version === null ? (
          "not checked yet"
        ) : (
          <Version>{status.latest_version}</Version>
        )}
      </Fact>
      {status.previous_version !== null && (
        <Fact term="Kept for rollback">
          <Version>{status.previous_version}</Version>
        </Fact>
      )}
      {skipped.length > 0 && (
        <Fact term="Skipped">
          <Version>{skipped.join(", ")}</Version>
        </Fact>
      )}
      <Fact term="Last check">
        {status.last_check === null
          ? "none yet"
          : `${formatDateTime(status.last_check)}${
              status.last_result === null ? "" : `: ${RESULTS[status.last_result]}`
            }`}
      </Fact>
      <Fact term="Next check">
        {status.next_check === null ? "none planned" : formatDateTime(status.next_check)}
      </Fact>
      <Fact term="Checks every">{formatSeconds(status.check_every_seconds)}</Fact>
      <Fact term="Updates itself">
        {status.can_update_itself ? "yes" : `no: ${status.why_not ?? "this install doesn't"}`}
      </Fact>
    </dl>
  );
}

/**
 * What open-ferry's own updates are doing, as the server's update status
 * says, with a check now. The mode is changed on the Settings tab.
 */
export function UpdatesCard({ className }: { className?: string }) {
  const status = useUpdateStatus();
  const call = useApiCall();
  const client = useQueryClient();
  const check = useMutation({
    mutationFn: () => call<CheckStarted>(UPDATE_CHECK, { method: "POST" }),
    onSettled: () => client.invalidateQueries({ queryKey: apiQueryKey(UPDATE) }),
  });
  const data = status.data;
  const checking = check.isPending || data?.checking === true;
  const canCheck = data !== undefined && data.mode !== "off" && data.trusts_release_key;

  return (
    <Card
      title="Updates"
      description="Whether open-ferry keeps itself up to date, and what it last found."
      className={className}
      actions={
        canCheck ? (
          <Button
            size="sm"
            disabled={checking}
            onClick={() => {
              check.mutate();
            }}
          >
            {checking ? <Spinner /> : <RefreshCw aria-hidden="true" className="size-4" />}
            {checking ? "Checking…" : "Check now"}
          </Button>
        ) : undefined
      }
    >
      {status.isPending && <Loading>Asking the server…</Loading>}
      {status.isError &&
        (isUpdatesUnavailable(status.error) ? (
          <p className="text-muted">
            This server doesn&apos;t check for updates, as one the terminal dashboard (
            <Code>open-ferry -tui</Code>) started never does.
          </p>
        ) : (
          <ProblemNotice
            problem={callProblem(status.error)}
            action={
              <Button
                size="sm"
                onClick={() => {
                  void status.refetch();
                }}
              >
                <RotateCw aria-hidden="true" className="size-4" />
                Try again
              </Button>
            }
          />
        ))}
      {data !== undefined && (
        <>
          <Notices status={data} />
          <Facts status={data} />
          {data.mode === "off" && (
            <p className="max-w-prose text-muted">
              Updates are off, so the server makes no update request. Turn them on in{" "}
              <Link to="/settings">Settings</Link>, or run <Code>open-ferry update -check</Code>{" "}
              on the server&apos;s computer to check by hand.
            </p>
          )}
          {data.mode !== "off" && (
            <p className="max-w-prose text-muted">
              Turn updates off, or to notify only, in <Link to="/settings">Settings</Link>.{" "}
              <a href={UPDATES_DOC_URL} rel="noreferrer" target="_blank">
                How updates work
              </a>
              .
            </p>
          )}
        </>
      )}
      {check.isError &&
        (isUpdatesOff(check.error) ? (
          <Alert tone="warn" live title="Updates are off">
            <p>Nothing was checked: updates were turned off before the check could start.</p>
          </Alert>
        ) : (
          <ProblemNotice problem={callProblem(check.error)} live />
        ))}
    </Card>
  );
}
