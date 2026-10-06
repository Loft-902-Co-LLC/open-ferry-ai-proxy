import { useMutation } from "@tanstack/react-query";
import { Gauge } from "lucide-react";
import { useState } from "react";

import { callProblem } from "../../api/access";
import { QUOTA_FETCH, type Credential, type QuotaAnswer } from "../../api/credentials";
import { useApiCall } from "../../api/hooks";
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

/** A button that asks the provider for a credential's quota, and shows it. */
export function QuotaButton({ credential }: { credential: Credential }) {
  const call = useApiCall();
  const [open, setOpen] = useState(false);
  const fetchQuota = useMutation({
    mutationFn: () =>
      call<QuotaAnswer>(QUOTA_FETCH, {
        method: "POST",
        json: { auth_index: credential.auth_index },
      }),
  });
  return (
    <>
      <Button
        size="sm"
        disabled={fetchQuota.isPending}
        onClick={() => {
          setOpen(true);
          fetchQuota.mutate();
        }}
      >
        {fetchQuota.isPending ? <Spinner /> : <Gauge aria-hidden="true" className="size-4" />}
        Check quota
      </Button>
      <Dialog
        open={open}
        title={`Quota of ${credential.name}`}
        onClose={() => {
          setOpen(false);
        }}
        footer={
          <Button
            data-autofocus
            onClick={() => {
              setOpen(false);
            }}
          >
            Close
          </Button>
        }
      >
        {fetchQuota.isPending && <Loading>Asking the provider…</Loading>}
        {fetchQuota.isError && <ProblemNotice problem={callProblem(fetchQuota.error)} live />}
        {fetchQuota.isSuccess && <QuotaView quota={fetchQuota.data} />}
      </Dialog>
    </>
  );
}
