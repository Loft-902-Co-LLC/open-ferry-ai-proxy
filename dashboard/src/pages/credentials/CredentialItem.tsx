import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Power, PowerOff, RotateCcw, Trash2 } from "lucide-react";
import { useId, useState, type ReactNode } from "react";

import { callProblem, type CallProblem } from "../../api/access";
import {
  AUTH_FILES,
  AUTH_FILE_STATUS,
  RESET_COOLDOWN,
  type Cooldown,
  type Credential,
  type ResetAnswer,
  type StatusAnswer,
} from "../../api/credentials";
import { useApiCall } from "../../api/hooks";
import { Badge } from "../../components/Badge";
import { Button } from "../../components/Button";
import { ConfirmDialog } from "../../components/Dialog";
import { ProblemNotice } from "../../components/ProblemNotice";
import { SecretText } from "../../components/SecretText";
import { Spinner } from "../../components/Spinner";
import {
  formatInteger,
  formatPercent,
  formatSeconds,
  formatShortDateTime,
} from "../../lib/format";
import { QuotaButton } from "./QuotaDialog";
import { DetailsButton, useOpenWhenTargeted } from "./TriageList";
import {
  canReset,
  credentialCooldowns,
  credentialHealth,
  explainReason,
  lastUsed,
  modelCooldowns,
  providerName,
  quotaCheckOf,
  quotaCheckText,
  resetLabel,
  resetRetries,
  timeLeft,
  unlistedQuotaChecks,
} from "./credentialStates";
import { quotaReadings, type QuotaReadings, type QuotaWindow } from "./quotaReadings";

/**
 * What the server did when it reset `named` and `models` of its models:
 * ended their rest, or, when `retried`, put a failing one back in use.
 * Either way the next request may use them.
 */
export function resetNotice(named: ReactNode, models: number, retried: boolean): ReactNode {
  const state = retried ? "back in use" : "no longer resting";
  return models === 0 ? (
    <>
      {named} is {state}: the server tries it again with the next request.
    </>
  ) : (
    <>
      {named} and {formatInteger(models)} {models === 1 ? "model" : "models"} are {state}: the
      server tries them again with the next request.
    </>
  );
}

/** "Back in about 4 min, at Oct 5, 12:04." */
export function backIn(cooldown: Cooldown): string {
  return `Back in ${timeLeft(cooldown.remaining_seconds)}, at ${formatShortDateTime(cooldown.retry_at)}.`;
}

/**
 * When `credential`'s `cooldown` ends, or, when the cap on quota rests
 * holds it, when the server next checks it, as of `readAt`, when the list
 * was made.
 */
export function restEnd(cooldown: Cooldown, credential: Credential, readAt: number): string {
  const check = quotaCheckOf(credential, cooldown);
  return check === undefined ? backIn(cooldown) : quotaCheckText(check, readAt);
}

/**
 * Why `resting`, the rest of the whole credential if it has one, and when
 * it ends; then the capped quota rests no cooldown shows, such as one whose
 * check is due, each with the model it covers.
 */
export function RestText({
  credential,
  resting,
  readAt,
}: {
  credential: Credential;
  resting: Cooldown | undefined;
  readAt: number;
}) {
  return (
    <>
      {resting !== undefined && (
        <p className="text-muted">
          {explainReason(resting.reason).meaning} {restEnd(resting, credential, readAt)}
        </p>
      )}
      {unlistedQuotaChecks(credential).map((check) => (
        <p key={`${check.scope}-${check.model_key ?? ""}`} className="text-muted">
          {check.scope === "model" && (
            <>
              <span className="font-mono text-[0.85em] break-all">
                {check.model_key ?? "A model"}
              </span>
              :{" "}
            </>
          )}
          {quotaCheckText(check, readAt)}
        </p>
      ))}
    </>
  );
}

/**
 * The name a credential shows under: the account's email for a sign-in,
 * else its file's name (or its ID, when it has no file).
 */
export function credentialDisplayName(credential: Credential): string {
  if (credential.account_type === "api_key") {
    return credential.name;
  }
  const account = (credential.account ?? credential.email ?? "").trim();
  return account === "" ? credential.name : account;
}

function tally(names: readonly string[]): Map<string, number> {
  const counts = new Map<string, number>();
  for (const name of names) {
    counts.set(name, (counts.get(name) ?? 0) + 1);
  }
  return counts;
}

/**
 * The names a list of credentials shows under, one each: the display name,
 * with the provider where two share an account ("ada@example.com (Codex)"),
 * and with the file's name where that still isn't enough.
 */
export function credentialNames(credentials: readonly Credential[]): string[] {
  const plain = credentials.map((credential) => ({
    credential,
    name: credentialDisplayName(credential),
  }));
  const plainCounts = tally(plain.map(({ name }) => name));
  const withProvider = plain.map(({ credential, name }) => ({
    credential,
    name,
    full:
      (plainCounts.get(name) ?? 0) > 1 && name !== credential.name
        ? `${name} (${providerName(credential.provider)})`
        : name,
  }));
  const fullCounts = tally(withProvider.map(({ full }) => full));
  return withProvider.map(({ credential, name, full }) =>
    (fullCounts.get(full) ?? 0) > 1 && name !== credential.name
      ? `${name} (${credential.name})`
      : full,
  );
}

/** The plan its sign-in says it has, as "Pro plan", or null. */
export function planOf(credential: Credential): string | null {
  const plan = credential.id_token?.plan_type?.trim() ?? "";
  return plan === "" ? null : `${plan.charAt(0).toUpperCase()}${plan.slice(1)} plan`;
}

/**
 * `credential`'s models resting, grouped by why, each group with what to
 * do, and each model with when its rest ends or is checked, as of `readAt`.
 */
export function ModelCooldowns({ credential, readAt }: { credential: Credential; readAt: number }) {
  const groups = new Map<string, Cooldown[]>();
  for (const cooldown of modelCooldowns(credential)) {
    const group = groups.get(cooldown.reason) ?? [];
    group.push(cooldown);
    groups.set(cooldown.reason, group);
  }
  return (
    <div className="space-y-1">
      <h4 className="font-medium">Models resting</h4>
      <div className="divide-y divide-line">
        {[...groups].map(([reasonCode, group]) => {
          const reason = explainReason(reasonCode);
          return (
            <div key={reasonCode} className="space-y-1 py-2 first:pt-0 last:pb-0">
              <p className="font-medium">{reason.title}</p>
              <p className="text-muted">
                {reason.meaning} {reason.action}
              </p>
              <ul className="space-y-0.5">
                {[...group]
                  .sort((a, b) => a.remaining_seconds - b.remaining_seconds)
                  .map((cooldown) => (
                    <li key={cooldown.model_key ?? cooldown.retry_at}>
                      <span className="font-mono text-[0.85em] break-all">
                        {cooldown.model_key ?? "A model"}
                      </span>
                      : {restEnd(cooldown, credential, readAt)}
                    </li>
                  ))}
              </ul>
            </div>
          );
        })}
      </div>
    </div>
  );
}

/** "53% used, starts over Oct 9, 08:00." */
function windowUse(window: QuotaWindow): string {
  const use = window.usedUp
    ? "Used up"
    : window.used === null
      ? ""
      : `${formatPercent(window.used)} used`;
  const reset =
    window.resetsAt === null ? "" : `starts over ${formatShortDateTime(window.resetsAt.toISOString())}`;
  const text = [use, reset].filter((part) => part !== "").join(", ");
  return text === "" ? "No reading." : `${text.charAt(0).toUpperCase()}${text.slice(1)}.`;
}

/** Each quota window, as the provider's last response gave it. */
export function QuotaWindows({ readings }: { readings: QuotaReadings }) {
  return (
    <div className="space-y-1">
      <h4 className="font-medium">Quota</h4>
      <dl className="grid gap-x-6 gap-y-0.5 sm:grid-cols-[max-content_1fr]">
        {readings.windows.map((window, index) => (
          <div key={`${window.name}-${String(index)}`} className="contents">
            <dt className="text-muted">{window.name}</dt>
            <dd className="tabular-nums">
              {windowUse(window)}
              {window === readings.limiting && " This is the limit that stops it."}
            </dd>
          </div>
        ))}
      </dl>
      <p className="text-muted">
        As the provider said at {formatShortDateTime(readings.observedAt.toISOString())}.
      </p>
    </div>
  );
}

/**
 * What its heading doesn't say of the account: an API key's key, shown
 * masked, and the file's name when it doesn't already name the account.
 */
function Account({ credential, name }: { credential: Credential; name: string }) {
  const key = credential.account_type === "api_key" ? (credential.account ?? "") : "";
  const file = credential.source === "file" && !credential.name.includes(name);
  if (key === "" && !file) {
    return null;
  }
  return (
    <div className="space-y-0.5">
      {key !== "" && (
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-muted">Key:</span>
          <SecretText value={key} label={`the key of ${credential.name}`} />
        </div>
      )}
      {file && (
        <p>
          <span className="text-muted">File:</span>{" "}
          <span className="break-all">{credential.name}</span>
        </p>
      )}
    </div>
  );
}

/** Its requests: in all, and in the recent windows the server keeps. */
export function Requests({ credential }: { credential: Credential }) {
  const recent = credential.recent_requests ?? [];
  const recentSuccess = recent.reduce((sum, bucket) => sum + bucket.success, 0);
  const recentFailed = recent.reduce((sum, bucket) => sum + bucket.failed, 0);
  return (
    <dl className="grid gap-x-6 gap-y-0.5 sm:grid-cols-[max-content_1fr]">
      <dt className="text-muted">Requests since the server started</dt>
      <dd className="tabular-nums">
        {formatInteger(credential.success)} succeeded, {formatInteger(credential.failed)} failed
      </dd>
      {recent.length > 0 && (
        <>
          <dt className="text-muted">In the last {formatSeconds(recent.length * 600)}</dt>
          <dd className="tabular-nums">
            {formatInteger(recentSuccess)} succeeded, {formatInteger(recentFailed)} failed
          </dd>
        </>
      )}
      {credential.last_refresh !== undefined && credential.last_refresh !== "" && (
        <>
          <dt className="text-muted">Token refreshed</dt>
          <dd>{formatShortDateTime(credential.last_refresh)}</dd>
        </>
      )}
    </dl>
  );
}

/** "Claude sign-in · Pro plan · Last used about 30 min ago". */
function Facts({ credential, name }: { credential: Credential; name: string }) {
  const kind =
    credential.account_type === "api_key"
      ? " API key"
      : credential.account_type === "oauth"
        ? " sign-in"
        : "";
  const memory =
    credential.source === "memory" || credential.runtime_only === true ? ", kept in memory only" : "";
  const label = credential.label?.trim() ?? "";
  const used = lastUsed(credential);
  const facts = [
    `${providerName(credential.provider)}${kind}${memory}`,
    label !== "" && label !== name && !credential.name.includes(label) ? label : null,
    planOf(credential),
    used === null ? null : `Last used ${used}`,
  ].filter((fact) => fact !== null);
  return <p className="text-muted">{facts.join(" · ")}</p>;
}

/** What the credential's health says, with why and what to do. */
function HealthText({ credential, readAt }: { credential: Credential; readAt: number }) {
  const health = credentialHealth(credential);
  return (
    <div className="space-y-1">
      <p>{health.summary}</p>
      <RestText
        credential={credential}
        resting={credentialCooldowns(credential)[0]}
        readAt={readAt}
      />
      {health.action !== null && (
        <p>
          <span className="font-medium">What to do:</span> {health.action}
        </p>
      )}
    </div>
  );
}

export interface CredentialItemProps {
  credential: Credential;
  /** The name it shows under, one of `credentialNames`. */
  name: string;
  /** Its element's id, which the address can point at. */
  anchor: string;
  /** In the folded group: a row whose details open on request. */
  compact: boolean;
  /** The address points at it: its details show. */
  targeted: boolean;
  /** When its list was made, in milliseconds since the epoch: what its times count from. */
  readAt: number;
  /**
   * Called with what an action did, for the list to say: the credential
   * may move in the list when its health changes, or leave it.
   */
  onDone: (notice: ReactNode) => void;
}

/** One credential: its health, why, what to do, and its actions. */
export function CredentialItem({
  credential,
  name,
  anchor,
  compact,
  targeted,
  readAt,
  onDone,
}: CredentialItemProps) {
  const titleId = useId();
  const detailsId = useId();
  const call = useApiCall();
  const client = useQueryClient();
  const [problem, setProblem] = useState<CallProblem | null>(null);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [open, setOpen] = useOpenWhenTargeted(targeted);
  const health = credentialHealth(credential);
  const models = modelCooldowns(credential);
  const readings = quotaReadings(credential);
  const off = credential.disabled || credential.status === "disabled";
  const named = <span className="font-medium break-all">{name}</span>;

  const refresh = () => client.invalidateQueries({ queryKey: [AUTH_FILES] });
  const failed = (error: unknown) => {
    setProblem(callProblem(error));
  };

  const toggle = useMutation({
    mutationFn: (disabled: boolean) =>
      call<StatusAnswer>(AUTH_FILE_STATUS, {
        method: "PATCH",
        json: { name: credential.name, auth_index: credential.auth_index, disabled },
      }),
    onSuccess: (answer) => {
      onDone(
        answer.disabled ? (
          <>Turned off {named}.</>
        ) : (
          <>Turned on {named}: the server uses it again.</>
        ),
      );
    },
    onError: failed,
    onSettled: refresh,
  });

  // Whether it was failing, so tried again, goes with the call: the list
  // may have been read again by the time the answer comes.
  const reset = useMutation<ResetAnswer, Error, boolean>({
    mutationFn: () =>
      call<ResetAnswer>(RESET_COOLDOWN, {
        method: "POST",
        json: { auth_index: credential.auth_index },
      }),
    onSuccess: (answer, retried) => {
      onDone(resetNotice(named, answer.models?.length ?? 0, retried));
    },
    onError: failed,
    onSettled: refresh,
  });

  const remove = useMutation({
    mutationFn: () =>
      call<unknown>(AUTH_FILES, { method: "DELETE", query: { name: credential.name } }),
    onSuccess: () => {
      setConfirmDelete(false);
      onDone(
        <>
          Deleted <span className="font-medium break-all">{credential.name}</span>.
        </>,
      );
    },
    onError: (error) => {
      setConfirmDelete(false);
      failed(error);
    },
    onSettled: refresh,
  });

  const busy = toggle.isPending || reset.isPending || remove.isPending;
  const itemName = (
    <>
      {" "}
      <span className="sr-only">{name}</span>
    </>
  );

  return (
    <article id={anchor} aria-labelledby={titleId} className="scroll-mt-4 space-y-3">
      <header className="flex items-start justify-between gap-2">
        <div className="min-w-0 flex-1 space-y-0.5">
          <h3 id={titleId} tabIndex={-1} data-anchor-heading className="font-semibold break-all">
            {name}
          </h3>
          <Facts credential={credential} name={name} />
        </div>
        <div className="flex shrink-0 items-center gap-2">
          <Badge tone={health.tone}>{health.label}</Badge>
          {compact && (
            <DetailsButton
              open={open}
              controls={detailsId}
              name={name}
              onToggle={() => {
                setOpen(!open);
              }}
            />
          )}
        </div>
      </header>

      <div id={detailsId} hidden={compact && !open} className="space-y-3">
        <HealthText credential={credential} readAt={readAt} />
        <Account credential={credential} name={name} />
        {readings !== null && <QuotaWindows readings={readings} />}
        {models.length > 0 && <ModelCooldowns credential={credential} readAt={readAt} />}
        <Requests credential={credential} />

        <div className="flex flex-wrap gap-2">
          <Button
            size="sm"
            disabled={busy}
            onClick={() => {
              setProblem(null);
              toggle.mutate(!off);
            }}
          >
            {toggle.isPending ? (
              <Spinner />
            ) : off ? (
              <Power aria-hidden="true" className="size-4" />
            ) : (
              <PowerOff aria-hidden="true" className="size-4" />
            )}
            {off ? "Turn on" : "Turn off"}
            {itemName}
          </Button>
          {canReset(credential) && (
            <Button
              size="sm"
              disabled={busy}
              onClick={() => {
                setProblem(null);
                reset.mutate(resetRetries(health));
              }}
            >
              {reset.isPending ? <Spinner /> : <RotateCcw aria-hidden="true" className="size-4" />}
              {resetLabel(health)}
              {itemName}
            </Button>
          )}
          {credential.supports_quota === true && <QuotaButton credential={credential} name={name} />}
          {credential.source === "file" && (
            <Button
              size="sm"
              variant="ghost"
              disabled={busy}
              onClick={() => {
                setProblem(null);
                setConfirmDelete(true);
              }}
            >
              <Trash2 aria-hidden="true" className="size-4" />
              Delete
              {itemName}
            </Button>
          )}
        </div>

        {problem !== null && <ProblemNotice problem={problem} live />}
      </div>

      <ConfirmDialog
        open={confirmDelete}
        title={`Delete ${credential.name}?`}
        confirmLabel="Delete file"
        pending={remove.isPending}
        onConfirm={() => {
          remove.mutate();
        }}
        onCancel={() => {
          setConfirmDelete(false);
        }}
      >
        <p>
          The server deletes its file and stops sending requests with it. To use the account again,
          sign in again or upload the file.
        </p>
      </ConfirmDialog>
    </article>
  );
}
