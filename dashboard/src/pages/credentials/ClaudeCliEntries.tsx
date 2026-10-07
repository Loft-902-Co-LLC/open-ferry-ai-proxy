import { useMutation, useQueryClient } from "@tanstack/react-query";
import { RotateCcw, UserCheck } from "lucide-react";
import { useId, useState } from "react";

import { callProblem, type CallProblem } from "../../api/access";
import { isApiError } from "../../api/client";
import { RESET_COOLDOWN, type ResetAnswer } from "../../api/credentials";
import {
  CLAUDE_CLI_AUTH_STATUS,
  CLAUDE_CLI_ENTRIES,
  type ClaudeCliAuthStatus,
  type ClaudeCliEntries as Entries,
  type ClaudeCliEntry,
} from "../../api/dashboard";
import { useApiCall, useApiQuery } from "../../api/hooks";
import { Alert } from "../../components/Alert";
import { Badge } from "../../components/Badge";
import { Button } from "../../components/Button";
import { Card } from "../../components/Card";
import { Code } from "../../components/Code";
import { CopyButton } from "../../components/CopyButton";
import { ProblemNotice } from "../../components/ProblemNotice";
import { QueryState } from "../../components/QueryState";
import { Spinner } from "../../components/Spinner";
import { formatInteger } from "../../lib/format";
import { claudeCommand, entriesUnserved, entryHealth } from "./claudeCli";
import { CREDENTIALS_REFRESH_MS } from "./CredentialList";
import { backIn, ModelCooldowns, QuotaWindows, Requests } from "./CredentialItem";
import { canReset, credentialCooldowns, explainReason, modelCooldowns } from "./credentialStates";
import { quotaReadings } from "./quotaReadings";

/** A command to run, with a button that copies it. */
function Command({ command, label }: { command: string; label: string }) {
  return (
    <div className="flex flex-wrap items-center gap-2">
      <Code>{command}</Code>
      <CopyButton text={command} label={label} />
    </div>
  );
}

/** What `auth-status` answered: whether it is signed in, and if not, what to run. */
function SignInStatus({ entry, status }: { entry: ClaudeCliEntry; status: ClaudeCliAuthStatus }) {
  const method = status.authMethod.trim();
  const how =
    method === "" ? null : (
      <>
        {" "}
        Sign-in method: <Code>{method}</Code>.
      </>
    );
  if (status.loggedIn) {
    return (
      <Alert tone="ok" live title="Signed in">
        <p>
          Claude Code is signed in.
          {how}
        </p>
      </Alert>
    );
  }
  return (
    <Alert tone="warn" live title="Not signed in">
      <p>
        Claude Code isn&apos;t signed in, so the server&apos;s requests with it fail.
        {how}
      </p>
      <p>
        <span className="font-medium">What to do:</span> on the server&apos;s computer, as the user
        open-ferry runs as, run this, then check again:
      </p>
      <Command
        command={claudeCommand(entry.config_dir, "auth login")}
        label={`Copy the sign-in command of ${entry.name}`}
      />
    </Alert>
  );
}

/** Why the check failed, with the failures of Claude Code itself explained. */
function CheckFailed({ entry, error }: { entry: ClaudeCliEntry; error: unknown }) {
  const detail = isApiError(error) ? error.detail : null;
  if (isApiError(error) && error.code === "claude_cli_timeout") {
    return (
      <Alert tone="warn" live title="Claude Code took too long to answer">
        <p>
          The server stopped it after 30 seconds. Try again; if it keeps happening, run this on the
          server&apos;s computer to see what it does:
        </p>
        <Command
          command={claudeCommand(entry.config_dir, "auth status")}
          label={`Copy the status command of ${entry.name}`}
        />
      </Alert>
    );
  }
  if (isApiError(error) && error.code === "claude_cli_failed") {
    return (
      <Alert tone="danger" live title="Claude Code couldn't be checked">
        <p>
          The server couldn&apos;t run the entry&apos;s Claude Code, or didn&apos;t understand its
          answer{detail === null ? "." : `: ${detail}`}
        </p>
        <p>
          Check that Claude Code is installed on the server&apos;s computer, where the entry&apos;s{" "}
          <Code>command</Code> says or on the server&apos;s <Code>PATH</Code>:{" "}
          <Code>claude --version</Code> says which version it is.
        </p>
      </Alert>
    );
  }
  return <ProblemNotice problem={callProblem(error)} live />;
}

type Notice = { tone: "ok"; text: string } | { tone: "problem"; problem: CallProblem };

/** One entry: its state, why, what to do, and its actions. */
function EntryItem({ entry }: { entry: ClaudeCliEntry }) {
  const titleId = useId();
  const call = useApiCall();
  const client = useQueryClient();
  const [notice, setNotice] = useState<Notice | null>(null);
  const credential = entry.credential;
  const health = entryHealth(entry);
  const resting = credential === null ? [] : credentialCooldowns(credential);
  const models = credential === null ? [] : modelCooldowns(credential);
  const readings = credential === null ? null : quotaReadings(credential);
  const error = entry.last_error;

  const reset = useMutation({
    mutationFn: (authIndex: string) =>
      call<ResetAnswer>(RESET_COOLDOWN, { method: "POST", json: { auth_index: authIndex } }),
    onSuccess: (answer) => {
      const count = answer.models?.length ?? 0;
      setNotice({
        tone: "ok",
        text:
          count === 0
            ? "Cooldown reset: the server tries it again with the next request."
            : `Cooldown reset for it and ${formatInteger(count)} ${count === 1 ? "model" : "models"}: the server tries it again with the next request.`,
      });
    },
    onError: (failure) => {
      setNotice({ tone: "problem", problem: callProblem(failure) });
    },
    onSettled: () => client.invalidateQueries({ queryKey: [CLAUDE_CLI_ENTRIES] }),
  });

  // Runs Claude Code on the server, so only when asked to.
  const check = useMutation({
    mutationFn: () =>
      call<ClaudeCliAuthStatus>(CLAUDE_CLI_AUTH_STATUS, { query: { name: entry.name } }),
  });

  return (
    <article aria-labelledby={titleId} className="space-y-3 rounded-md border border-line p-4">
      <header className="flex flex-wrap items-start justify-between gap-2">
        <div className="min-w-0 space-y-0.5">
          <h3 id={titleId} className="font-semibold break-all">
            {entry.name}
          </h3>
          <p className="text-muted">Claude Code, run by the server</p>
        </div>
        <Badge tone={health.tone}>{health.label}</Badge>
      </header>

      <dl className="grid gap-x-6 gap-y-0.5 sm:grid-cols-[max-content_1fr]">
        <dt className="text-muted">Prefix</dt>
        <dd>
          {entry.prefix === "" ? (
            "None"
          ) : (
            <>
              <Code>{entry.prefix}</Code>: calls to <Code>{`${entry.prefix}/<model>`}</Code> go
              to it
            </>
          )}
        </dd>
        <dt className="text-muted">Config directory</dt>
        <dd>
          {entry.config_dir === "" ? (
            "None: the server's CLAUDE_CONFIG_DIR, else Claude Code's default"
          ) : (
            <Code>{entry.config_dir}</Code>
          )}
        </dd>
      </dl>

      <div className="space-y-1">
        <p>{health.summary}</p>
        {resting[0] !== undefined && (
          <p className="text-muted">
            {explainReason(resting[0].reason).meaning} {backIn(resting[0])}
          </p>
        )}
        {health.action !== null && (
          <p>
            <span className="font-medium">What to do:</span> {health.action}
          </p>
        )}
      </div>

      {error !== null && (
        <div className="space-y-0.5">
          <h4 className="font-medium">Last error</h4>
          <p className="break-words">
            {error.message}
            {error.http_status !== null && (
              <span className="text-muted"> (HTTP {error.http_status})</span>
            )}
          </p>
        </div>
      )}
      {readings !== null && <QuotaWindows readings={readings} />}
      {models.length > 0 && <ModelCooldowns cooldowns={models} />}
      {credential !== null && <Requests credential={credential} />}

      <div className="flex flex-wrap gap-2">
        <Button
          size="sm"
          disabled={check.isPending}
          onClick={() => {
            check.mutate();
          }}
        >
          {check.isPending ? <Spinner /> : <UserCheck aria-hidden="true" className="size-4" />}
          Check sign-in
        </Button>
        {credential !== null && canReset(credential) && (
          <Button
            size="sm"
            disabled={reset.isPending}
            onClick={() => {
              setNotice(null);
              reset.mutate(credential.auth_index);
            }}
          >
            {reset.isPending ? <Spinner /> : <RotateCcw aria-hidden="true" className="size-4" />}
            Reset cooldown
          </Button>
        )}
      </div>

      {check.isSuccess && <SignInStatus entry={entry} status={check.data} />}
      {check.isError && <CheckFailed entry={entry} error={check.error} />}
      {notice?.tone === "ok" && (
        <Alert tone="ok" live>
          <p>{notice.text}</p>
        </Alert>
      )}
      {notice?.tone === "problem" && <ProblemNotice problem={notice.problem} live />}
    </article>
  );
}

/**
 * The config's `claude-cli` entries, each a Claude Code account the server
 * runs Claude Code with. A server without entries, or that doesn't list
 * them, gets no card.
 */
export function ClaudeCliEntries() {
  const query = useApiQuery<Entries>(CLAUDE_CLI_ENTRIES, undefined, {
    refetchInterval: CREDENTIALS_REFRESH_MS,
  });
  if (
    query.isPending ||
    (query.isError && entriesUnserved(query.error)) ||
    (query.isSuccess && query.data.entries.length === 0)
  ) {
    return null;
  }
  return (
    <Card
      title="Claude Code accounts"
      description="The claude-cli entries in config.yaml: each runs Claude Code, signed in with its own subscription. Add, change and remove them in config.yaml."
    >
      <QueryState query={query}>
        {(data) => (
          <div className="space-y-3">
            {data.entries.map((entry) => (
              <EntryItem key={entry.name} entry={entry} />
            ))}
          </div>
        )}
      </QueryState>
    </Card>
  );
}
