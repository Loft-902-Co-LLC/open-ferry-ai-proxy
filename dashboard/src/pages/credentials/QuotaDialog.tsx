import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Gauge } from "lucide-react";
import { useEffect, useRef, useState } from "react";

import { callProblem } from "../../api/access";
import { AUTH_FILES, QUOTA_FETCH, type Credential, type QuotaAnswer } from "../../api/credentials";
import { useApiCall } from "../../api/hooks";
import { Alert } from "../../components/Alert";
import { Button } from "../../components/Button";
import { Dialog } from "../../components/Dialog";
import { ProblemNotice } from "../../components/ProblemNotice";
import { Loading } from "../../components/QueryState";
import { Spinner } from "../../components/Spinner";
import { formatDateTime, formatPercent } from "../../lib/format";

function metricValue(metric: NonNullable<QuotaAnswer["summary"]>[number]): string {
  if (metric.format === "currency" && metric.currency !== undefined && metric.currency !== "") {
    return new Intl.NumberFormat("en", { style: "currency", currency: metric.currency }).format(
      metric.value,
    );
  }
  const value = new Intl.NumberFormat("en", { maximumFractionDigits: 2 }).format(metric.value);
  return metric.unit === undefined || metric.unit === "" ? value : `${value} ${metric.unit}`;
}

/** A quota answer: the plan, each window's share left, and the figures. */
export function QuotaView({ quota }: { quota: QuotaAnswer }) {
  const plan = [quota.subscription?.plan, quota.subscription?.tierName]
    .filter((part) => part !== undefined && part !== "")
    .join(", ");
  const groups = (quota.groups ?? []).filter((group) => (group.buckets ?? []).length > 0);
  const summary = quota.summary ?? [];
  return (
    <div className="space-y-3">
      {plan !== "" && (
        <p>
          <span className="text-muted">Plan:</span> {plan}
        </p>
      )}
      {groups.map((group, index) => (
        <section key={`${group.displayName ?? ""}-${String(index)}`} className="space-y-1">
          {group.displayName !== undefined && group.displayName !== "" && (
            <h3 className="font-semibold">{group.displayName}</h3>
          )}
          <ul className="space-y-1">
            {(group.buckets ?? []).map((bucket, bucketIndex) => (
              <li key={`${bucket.window ?? ""}-${String(bucketIndex)}`}>
                <span className="font-medium">{bucket.window ?? "Limit"}</span>:{" "}
                {formatPercent(bucket.remainingFraction)} left
                {bucket.resetTime !== undefined && bucket.resetTime !== "" && (
                  <>, resets {formatDateTime(bucket.resetTime)}</>
                )}
                {bucket.description !== undefined && bucket.description !== "" && (
                  <span className="block text-muted">{bucket.description}</span>
                )}
              </li>
            ))}
          </ul>
        </section>
      ))}
      {summary.length > 0 && (
        <dl className="grid gap-x-6 gap-y-1 sm:grid-cols-[max-content_1fr]">
          {summary.map((metric) => (
            <div key={metric.key} className="contents">
              <dt className="text-muted">{metric.label}</dt>
              <dd className="tabular-nums">{metricValue(metric)}</dd>
            </div>
          ))}
        </dl>
      )}
    </div>
  );
}

/** Report capacity only for the buckets actually returned, not for unknown scopes. */
export function quotaCooldownConflict(credential: Credential, quota: QuotaAnswer): boolean {
  const buckets = (quota.groups ?? []).flatMap((group) => group.buckets ?? []);
  const quotaRest =
    (credential.cooldowns ?? []).some(
      (cooldown) => cooldown.reason === "quota" || cooldown.reason === "credential_quota",
    ) || (credential.quota_checks ?? []).length > 0;
  return (
    quotaRest && buckets.length > 0 && buckets.every(
      (bucket) => Number.isFinite(bucket.remainingFraction) &&
        bucket.remainingFraction > 0 && bucket.remainingFraction <= 1,
    )
  );
}

/** The row only launches the quota read; the list owns its dialog across row moves. */
export function QuotaButton({ name, disabled, checking, onClick }: {
  name: string;
  disabled: boolean;
  checking: boolean;
  onClick: () => void;
}) {
  return (
    <Button size="sm" disabled={disabled} onClick={onClick}>
      {checking ? <Spinner /> : <Gauge aria-hidden="true" className="size-4" />}
      Check quota <span className="sr-only">of {name}</span>
    </Button>
  );
}

/** Keep the quota answer visible even when reconciliation moves or removes its row. */
export function useQuotaDialog(credentials: readonly Credential[], onLostOpener: () => void) {
  const call = useApiCall();
  const client = useQueryClient();
  const [target, setTarget] = useState<{ id: string; name: string } | null>(null);
  const opener = useRef<HTMLElement | null>(null);
  useEffect(() => {
    if (target === null && opener.current !== null) {
      const previous = opener.current;
      opener.current = null;
      // A recovered row can move into the hidden ready group while the dialog stays open.
      if (!previous.isConnected || previous.closest("[hidden]") !== null || previous.matches(":disabled")) {
        onLostOpener();
      }
    }
  }, [target, onLostOpener]);
  const fetchQuota = useMutation({
    mutationFn: async (credential: Credential) => {
      const quota = await call<QuotaAnswer>(QUOTA_FETCH, {
        method: "POST",
        json: { auth_index: credential.auth_index },
      });
      // Keep reload success with this answer, including when another read supersedes it.
      let reloadFailed = false;
      try {
        await client.invalidateQueries({ queryKey: [AUTH_FILES] }, { throwOnError: true });
      } catch {
        reloadFailed = true;
      }
      return { quota, reloadFailed };
    },
  });
  const current = credentials.find((credential) => credential.id === target?.id);
  const dialog = (
    <Dialog
      open={target !== null}
      title={`Quota of ${target?.name ?? "credential"}`}
      onClose={() => { setTarget(null); }}
      footer={
        <Button data-autofocus onClick={() => { setTarget(null); }}>
          Close
        </Button>
      }
    >
      {fetchQuota.isPending && <Loading>Asking the provider…</Loading>}
      {fetchQuota.isError && <ProblemNotice problem={callProblem(fetchQuota.error)} live />}
      {fetchQuota.isSuccess && (
        <>
          <QuotaView quota={fetchQuota.data.quota} />
          {fetchQuota.data.reloadFailed ? (
            <Alert tone="warn" live title="Routing state could not be reloaded">
              <p>
                The quota check succeeded, but the proxy routing state could not be reloaded.
                The cooldown shown on the page may be out of date. Reload the credentials before
                deciding whether to clear the proxy cooldown.
              </p>
            </Alert>
          ) : current !== undefined && quotaCooldownConflict(current, fetchQuota.data.quota) && (
            <Alert tone="warn" live title="Proxy quota cooldown still cached">
              <p>
                The quota check reports available capacity, but a proxy quota cooldown is still
                cached. The read may not confirm the limit that caused it. Clear proxy cooldown
                only if you have confirmed the affected limit has reset.
              </p>
              <p>Unknown or model-specific limits may not be included in this read.</p>
            </Alert>
          )}
        </>
      )}
    </Dialog>
  );
  return {
    dialog,
    pending: fetchQuota.isPending,
    checkingId: fetchQuota.isPending ? fetchQuota.variables.id : null,
    openQuota: (credential: Credential, name: string) => {
      // Quota probes are serialized, even if their dialog was closed mid-read.
      if (fetchQuota.isPending) return;
      opener.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
      setTarget({ id: credential.id, name });
      fetchQuota.mutate(credential);
    },
  };
}
