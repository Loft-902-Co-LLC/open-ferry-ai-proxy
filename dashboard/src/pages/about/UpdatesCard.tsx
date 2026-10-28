import { useMutation, useQueryClient } from "@tanstack/react-query";
import { RefreshCw, RotateCw } from "lucide-react";
import { useEffect, useState, type ReactNode } from "react";
import { Link } from "react-router";

import { callProblem } from "../../api/access";
import { isApiError } from "../../api/client";
import { apiQueryKey, useApiCall } from "../../api/hooks";
import {
  UPDATE,
  UPDATE_CHECK,
  UPDATE_INSTALL,
  UPDATE_ROLLBACK,
  isUpdatesOff,
  isUpdatesUnavailable,
  type CheckStarted,
  type UpdateModeSource,
  type UpdateResult,
  type UpdateStatus,
  type SwitchStarted,
} from "../../api/update";
import { useUpdateStatus } from "../../api/updateStatus";
import { Alert } from "../../components/Alert";
import { Badge, type BadgeTone } from "../../components/Badge";
import { Button } from "../../components/Button";
import { Card } from "../../components/Card";
import { Code } from "../../components/Code";
import { ConfirmDialog } from "../../components/Dialog";
import { ProblemNotice } from "../../components/ProblemNotice";
import { Loading } from "../../components/QueryState";
import { Spinner } from "../../components/Spinner";
import { formatDateTime, formatSeconds } from "../../lib/format";
import { SwitchStatus } from "./SwitchStatus";

type SwitchAction = { action: "install" } | { action: "rollback"; target: string };

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
    status.staged_version !== null && status.staged_version !== (status.supervisor?.running_version ?? status.installed_version)
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
      {status.restart_needed && status.supervisor === null && (
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
            It was downloaded and its signature checked. {status.supervisor !== null ?
            "Install now switches the running server to it without a restart." : <>To install it, run{" "}
            <Code>open-ferry update</Code> on the server&apos;s computer, then restart the
            server.</>}
          </p>
        </Alert>
      )}
      {staged === null && status.update_available && status.latest_version !== null && status.latest_version !== status.supervisor?.running_version && (
        <Alert tone="info" title={`open-ferry ${status.latest_version} is out`}>
          {status.supervisor !== null ? (
            <p>Install now downloads and checks the release, then switches the running server to it.</p>
          ) : !status.can_update_itself ? (
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
  const runningVersion = status.supervisor?.running_version ?? status.running_version;
  const checkedOtherChannel = status.checked_channel !== null && status.checked_channel !== status.channel;
  const checkBeforeSwitch = status.last_result === "up-to-date" && status.latest_version !== null && status.latest_version !== runningVersion;
  const rollbackTarget = status.supervisor?.rollback_target ?? status.previous_version;
  const skipped = [
    ...status.failed_versions,
    ...(status.rolled_back_version === null ? [] : [status.rolled_back_version]),
  ];
  return (
    <dl className="space-y-1.5">
      <Fact term="Updates">
        <Badge tone={updates.tone}>{updates.label}</Badge> {SOURCES[status.mode_source]}
      </Fact>
      <Fact term="Channel">{status.channel}</Fact>
      {status.checked_channel && status.checked_channel !== status.channel && (
        <Fact term="Last checked channel">{status.checked_channel}</Fact>
      )}
      {status.channel === "preview" && status.release_source && (
        <Fact term="Release source">{status.release_source}</Fact>
      )}
      {status.channel_error && <Fact term="Source warning">{status.channel_error}</Fact>}
      <Fact term="Running">
        <Version>{runningVersion}</Version>
      </Fact>
      {status.installed_version !== runningVersion && (
        <Fact term="Installed">
          <Version>{status.installed_version}</Version>
        </Fact>
      )}
      <Fact term="Latest release">
        {status.latest_version === null ? (
          "not checked yet"
        ) : (
          <><Version>{status.latest_version}</Version>{checkBeforeSwitch && " (last checked)"}</>
        )}
      </Fact>
      {rollbackTarget !== null && (
        <Fact term="Kept for rollback">
          <Version>{rollbackTarget}</Version>
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
              checkedOtherChannel ? `: checked on ${status.checked_channel}` : status.last_result === null ? "" : `: ${checkBeforeSwitch ? "checked before the running version changed" : RESULTS[status.last_result]}`
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
  const [confirm, setConfirm] = useState<SwitchAction | null>(null);
  const switching = useMutation({
    mutationFn: (request: SwitchAction) =>
      call<SwitchStarted>(request.action === "install" ? UPDATE_INSTALL : UPDATE_ROLLBACK, {
        method: "POST",
        json: request.action === "rollback" ? { target: request.target } : undefined,
      }),
    onMutate: async ({ action }) => {
      await client.cancelQueries({ queryKey: apiQueryKey(UPDATE) });
      const before = client.getQueryData<UpdateStatus>(apiQueryKey(UPDATE));
      const optimistic = { request_id: null, action, state: "preparing" as const, id: null, reason: null, pending: true };
      client.setQueryData<UpdateStatus>(apiQueryKey(UPDATE), (old) => old === undefined ? old : {
        ...old,
        switch_request: optimistic,
      });
      return {
        previousRead: status.lastRead ?? before,
        lastSwitch: JSON.stringify(before?.supervisor?.last_switch),
      };
    },
    onSettled: () => client.invalidateQueries({ queryKey: apiQueryKey(UPDATE) }),
  });
  const { isError: switchError, reset: resetSwitch, context: switchContext } = switching;
  const alreadyLatest = isApiError(switching.error) && switching.error.code === "already_latest";
  const switchUnconfirmed = switching.isError && callProblem(switching.error).kind === "unreachable";
  const readErrorVisible = status.isError && !status.hidePollError;
  const latestRequest = status.lastRead?.switch_request;
  const lastSwitch = status.lastRead?.supervisor?.last_switch;
  const liveSwitch = status.lastRead?.supervisor?.switch;
  useEffect(() => {
    if (!switchError || switchContext === undefined || status.lastRead === switchContext.previousRead) return;
    if ((lastSwitch != null && JSON.stringify(lastSwitch) !== switchContext.lastSwitch) ||
      (latestRequest?.action === switching.variables.action &&
        latestRequest.request_id !== switchContext.previousRead?.switch_request?.request_id) ||
      (liveSwitch?.how === (switching.variables.action === "install" ? "update" : "rollback") &&
        liveSwitch.id !== switchContext.previousRead?.supervisor?.switch?.id)) {
      resetSwitch();
    }
  }, [switchError, switchContext, status.lastRead, latestRequest, lastSwitch, liveSwitch, resetSwitch, switching.variables]);
  const supervisor = data?.supervisor;
  const preparing = switching.isPending || data?.switch_request?.pending === true;
  const stagedVersion = data?.staged_version;
  const runningVersion = supervisor?.running_version ?? data?.running_version;
  const available = (stagedVersion != null && stagedVersion !== runningVersion) ||
    (data?.update_available === true && data.latest_version !== runningVersion);
  const enabled = !status.isError && supervisor?.switching_disabled === null && supervisor.error === null && !supervisor.stale && !preparing;
  const trustsReleaseKey = data?.trusts_release_key === true;
  const canInstall = enabled && supervisor.phase === "idle" && available && trustsReleaseKey;
  const canRollback = enabled && ["idle", "watching"].includes(supervisor.phase) && supervisor.rollback_target !== null;
  const canConfirm = confirm?.action === "rollback" ? canRollback && confirm.target === supervisor.rollback_target : canInstall;
  const installVersion = data?.latest_version ?? stagedVersion;
  const failedBefore = installVersion != null && data?.failed_versions.includes(installVersion);
  const rolledBackBefore = installVersion != null && data?.rolled_back_version === installVersion;
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
      {readErrorVisible &&
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
          <SwitchStatus polling={status.polling} recoveringSupervisor={status.recoveringSupervisor} status={data} onRetry={() => { void status.refetch(); }} hidePhase={readErrorVisible} hideRetry={switchUnconfirmed} />
          {(canInstall || canRollback) && <div className="flex flex-wrap gap-2">
            {canInstall && <Button variant="primary" onClick={() => { setConfirm({ action: "install" }); }}>Install now</Button>}
            {canRollback && <Button onClick={() => {
              if (supervisor.rollback_target !== null) setConfirm({ action: "rollback", target: supervisor.rollback_target });
            }}>Roll back</Button>}
          </div>}
          <Facts status={data} />
          {data.supervisor === null && <p className="max-w-prose text-muted">
            To update this server, run <Code>open-ferry update</Code> on its computer, then restart it.
          </p>}
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
      {switching.isError && <Alert tone={alreadyLatest ? "info" : "warn"} live title={alreadyLatest ? "Already running the latest release" : switchUnconfirmed ? "The switch could not be confirmed" : "The switch request failed"}>
        <p>{switchUnconfirmed ?
          (status.polling ? "The server did not answer. Checking its status; the request will not be sent again." :
            readErrorVisible ? "The server did not answer. Choose Try again to see whether it started. The request will not be sent again." :
            "The server did not answer. Choose Read status to see whether it started. The request will not be sent again.") :
          isApiError(switching.error) ? switching.error.detail ?? switching.error.message : "The server refused the switch."}</p>
        {switchUnconfirmed && !status.polling && !readErrorVisible && <Button size="sm" onClick={() => { void status.refetch(); }}>Read status</Button>}
      </Alert>}
      <ConfirmDialog
        open={confirm !== null}
        title={confirm?.action === "rollback" ? `Roll back to ${confirm.target}?` : "Install the newer release now?"}
        confirmLabel={confirm?.action === "rollback" ? "Roll back" : "Install now"}
        tone="primary"
        confirmDisabled={!canConfirm}
        onCancel={() => { setConfirm(null); }}
        onConfirm={() => {
          if (confirm !== null && canConfirm) {
            switching.mutate(confirm);
            setConfirm(null);
          }
        }}
      >
        <p>The supervisor starts the {confirm?.action === "rollback" ? "previous" : "new"} version and hands the running server over to it. Existing requests can finish.</p>
        {!canConfirm && <p>This is no longer available. Close this and read the status.</p>}
        {confirm?.action === "install" && failedBefore && <p>
          Version {installVersion} failed here before. Install it again?
        </p>}
        {confirm?.action === "install" && rolledBackBefore && <p>
          Version {installVersion} was rolled back from. Install it again?
        </p>}
      </ConfirmDialog>
    </Card>
  );
}
