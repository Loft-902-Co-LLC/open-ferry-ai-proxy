import type { UpdateStatus } from "../../api/update";
import { switchInProgress } from "../../api/updateStatus";
import { Alert } from "../../components/Alert";
import { Button } from "../../components/Button";
import { formatDateTime } from "../../lib/format";

function readable(word: string): string {
  return word.replaceAll("-", " ");
}

/** The supervisor's current work and its durable last outcome. */
export function SwitchStatus({ status, onRetry, hidePhase = false, hideRetry = false, recoveringSupervisor = false, polling = false }: { status: UpdateStatus; onRetry: () => void; hidePhase?: boolean; hideRetry?: boolean; recoveringSupervisor?: boolean; polling?: boolean }) {
  const supervisor = status.supervisor;
  const request = status.switch_request;
  const last = supervisor?.last_switch;
  const outcome = last == null ? null :
    last.result === "switched" ? `Switched to ${last.to}` :
    last.result === "rolled-back" ? `Rolled back to ${last.to}` :
    `${readable(last.result)}: ${last.to}`;
  return <>
    {!hidePhase && request?.state === "preparing" && <Alert tone="info" live title="Preparing the switch">
      <p>The server is preparing the {request.action === "install" ? "release" : "rollback"}. This can take a few minutes.</p>
    </Alert>}
    {!hidePhase && request?.state === "accepted" && request.pending && <Alert tone="info" live title="Switch accepted">
      <p>Waiting for the supervisor&apos;s status.</p>
    </Alert>}
    {request?.reason != null && <Alert tone={request.code === "already_latest" ? "info" : "warn"} live title={request.code === "already_latest" ? "Already running the latest release" : request.state === "refused" ? "Switch refused" : request.state === "failed" ? "Switch failed" : "The switch could not be confirmed"}>
      <p className="break-words">{request.reason}</p>
      {!hidePhase && !hideRetry && request.state === "unknown" && request.pending && supervisor?.error == null && <Button size="sm" onClick={onRetry}>Read status</Button>}
    </Alert>}
    {!hidePhase && supervisor?.error === null && switchInProgress({ ...status, switch_request: null }) && <Alert tone="info" live title={supervisor.switch === null && ["switching", "stopping"].includes(supervisor.phase) ?
      `The supervisor is ${supervisor.phase}` : `Switch: ${readable(supervisor.phase)}`}>
      {supervisor.switch != null && <p>{supervisor.switch.from} to {supervisor.switch.to}.</p>}
      {supervisor.watch_until !== null && <p>Watching until {formatDateTime(supervisor.watch_until)}.</p>}
    </Alert>}
    {!hidePhase && recoveringSupervisor && supervisor?.error === null && <Alert tone="warn" live title="The supervisor is recovering">
      <p>{polling ? "Reading its status." : "Choose Read status to see whether it has recovered."}</p>
      {!polling && !hideRetry && <Button size="sm" onClick={onRetry}>Read status</Button>}
    </Alert>}
    {supervisor?.error != null && <Alert tone="warn" live title="The supervisor status could not be read">
      <p>{supervisor.error}</p>
      {!hidePhase && !hideRetry && <Button size="sm" onClick={onRetry}>Try again</Button>}
    </Alert>}
    {supervisor?.switching_disabled != null && <Alert tone="warn" title="Switching is unavailable">
      <p className="break-words">{supervisor.switching_disabled}</p>
    </Alert>}
    {supervisor?.notes != null && supervisor.notes.length > 0 && <Alert tone="warn" title="Some switch records could not be read">
      {supervisor.notes.map((note) => <p key={note}>{note}</p>)}
    </Alert>}
    {last != null && <Alert tone={last.reason === null ? "info" : "warn"} title={outcome ?? "Last switch"}>
      <p>{last.auto === true && "Automatic switch. "}{last.at !== null && formatDateTime(last.at)}</p>
      {last.reason !== null && <p className="break-words">{last.reason}</p>}
    </Alert>}
  </>;
}
