import type { UpdateStatus } from "../../api/update";
import { switchInProgress } from "../../api/updateStatus";
import { Alert } from "../../components/Alert";
import { Button } from "../../components/Button";
import { formatDateTime } from "../../lib/format";

function readable(word: string): string {
  return word.replaceAll("-", " ");
}

/** The supervisor's current work and its durable last outcome. */
export function SwitchStatus({ status, onRetry, hidePhase = false }: { status: UpdateStatus; onRetry: () => void; hidePhase?: boolean }) {
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
    {request?.reason != null && <Alert tone="warn" live title={request.state === "refused" ? "Switch refused" : request.state === "failed" ? "Switch failed" : "The switch could not be confirmed"}>
      <p className="break-words">{request.reason}</p>
      {!hidePhase && request.state === "unknown" && request.pending && supervisor?.error == null && <Button size="sm" onClick={onRetry}>Read status</Button>}
    </Alert>}
    {!hidePhase && supervisor?.error === null && switchInProgress({ ...status, switch_request: null }) && <Alert tone="info" live title={`Switch: ${readable(supervisor.phase)}`}>
      {supervisor.switch != null && <p>{supervisor.switch.from} to {supervisor.switch.to}.</p>}
      {supervisor.watch_until !== null && <p>Watching until {formatDateTime(supervisor.watch_until)}.</p>}
    </Alert>}
    {supervisor?.error != null && <Alert tone="warn" live title="The supervisor status could not be read">
      <p>{supervisor.error}</p>
      {!hidePhase && <Button size="sm" onClick={onRetry}>Try again</Button>}
    </Alert>}
    {supervisor?.switching_disabled != null && <Alert tone="warn" title="Switching is unavailable">
      <p className="break-words">{supervisor.switching_disabled}</p>
    </Alert>}
    {last != null && <Alert tone={last.reason === null ? "info" : "warn"} title={outcome ?? "Last switch"}>
      <p>{last.auto === true && "Automatic switch. "}{last.at !== null && formatDateTime(last.at)}</p>
      {last.reason !== null && <p className="break-words">{last.reason}</p>}
    </Alert>}
  </>;
}
