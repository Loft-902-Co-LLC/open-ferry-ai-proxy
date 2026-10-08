import { useMutation, useQueryClient } from "@tanstack/react-query";
import { RotateCcw, UserCheck } from "lucide-react";
import { useId, useState, type ReactNode } from "react";

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
import { Spinner } from "../../components/Spinner";
import { claudeCliAnchor } from "./anchors";
import { claudeCommand, entriesUnserved, entryHealth } from "./claudeCli";
import { pollWhileRead } from "./CredentialList";
import {
  backIn,
  ModelCooldowns,
  QuotaWindows,
  Requests,
  resetNotice,
} from "./CredentialItem";
import {
  canReset,
  credentialCooldowns,
  explainReason,
  lastUsed,
  modelCooldowns,
  resetLabel,
  resetRetries,
  secondsUntilBack,
} from "./credentialStates";
import { CheckedAt, PolledState } from "./PolledState";
import { quotaReadings } from "./quotaReadings";
import { DetailsButton, TriageList, useOpenWhenTargeted, type TriageEntry } from "./TriageList";

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

interface EntryItemProps {
  entry: ClaudeCliEntry;
  /** Its element's id, which the address can point at. */
  anchor: string;
  /** In the folded group: a row whose details open on request. */
  compact: boolean;
  /** The address points at it: its details show. */
  targeted: boolean;
  /** Called with what an action did, for the list to say. */
  onDone: (notice: ReactNode) => void;
}

/** One entry: its state, why, what to do, and its actions. */
function EntryItem({ entry, anchor, compact, targeted, onDone }: EntryItemProps) {
  const titleId = useId();
  const detailsId = useId();
  const call = useApiCall();
  const client = useQueryClient();
  const [problem, setProblem] = useState<CallProblem | null>(null);
  const [open, setOpen] = useOpenWhenTargeted(targeted);
  const credential = entry.credential;
  const health = entryHealth(entry);
  const resting = credential === null ? [] : credentialCooldowns(credential);
  const models = credential === null ? [] : modelCooldowns(credential);
  const readings = credential === null ? null : quotaReadings(credential);
  const error = entry.last_error;

  // Whether it was failing, so tried again, goes with the call: the list
  // may have been read again by the time the answer comes.
  const reset = useMutation({
    mutationFn: ({ authIndex }: { authIndex: string; retried: boolean }) =>
      call<ResetAnswer>(RESET_COOLDOWN, { method: "POST", json: { auth_index: authIndex } }),
    onSuccess: (answer, { retried }) => {
      const named = <span className="font-medium break-all">{entry.name}</span>;
      onDone(resetNotice(named, answer.models?.length ?? 0, retried));
    },
    onError: (failure) => {
      setProblem(callProblem(failure));
    },
    onSettled: () => client.invalidateQueries({ queryKey: [CLAUDE_CLI_ENTRIES] }),
  });

  // Runs Claude Code on the server, so only when asked to.
  const check = useMutation({
    mutationFn: () =>
      call<ClaudeCliAuthStatus>(CLAUDE_CLI_AUTH_STATUS, { query: { name: entry.name } }),
  });

  const used = credential === null ? null : lastUsed(credential);
  const itemName = (
    <>
      {" "}
      <span className="sr-only">{entry.name}</span>
    </>
  );

  return (
    <article id={anchor} aria-labelledby={titleId} className="scroll-mt-4 space-y-3">
      <header className="flex items-start justify-between gap-2">
        <div className="min-w-0 flex-1 space-y-0.5">
          <h3 id={titleId} tabIndex={-1} data-anchor-heading className="font-semibold break-all">
            {entry.name}
          </h3>
          <p className="text-muted">
            Claude Code, run by the server{used === null ? "" : ` · Last used ${used}`}
          </p>
        </div>
        <div className="flex shrink-0 items-center gap-2">
          <Badge tone={health.tone}>{health.label}</Badge>
          {compact && (
            <DetailsButton
              open={open}
              controls={detailsId}
              name={entry.name}
              onToggle={() => {
                setOpen(!open);
              }}
            />
          )}
        </div>
      </header>

      <div id={detailsId} hidden={compact && !open} className="space-y-3">
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
            {itemName}
          </Button>
          {credential !== null && canReset(credential) && (
            <Button
              size="sm"
              disabled={reset.isPending}
              onClick={() => {
                setProblem(null);
                reset.mutate({
                  authIndex: credential.auth_index,
                  retried: resetRetries(health),
                });
              }}
            >
              {reset.isPending ? <Spinner /> : <RotateCcw aria-hidden="true" className="size-4" />}
              {resetLabel(health)}
              {itemName}
            </Button>
          )}
        </div>

        {check.isSuccess && <SignInStatus entry={entry} status={check.data} />}
        {check.isError && <CheckFailed entry={entry} error={check.error} />}
        {problem !== null && <ProblemNotice problem={problem} live />}
      </div>
    </article>
  );
}

interface Entry extends TriageEntry {
  entry: ClaudeCliEntry;
}

function entryOf(entry: ClaudeCliEntry): Entry {
  return {
    key: entry.name,
    anchor: claudeCliAnchor(entry.name),
    name: entry.name,
    health: entryHealth(entry),
    backIn: secondsUntilBack(entry.credential),
    entry,
  };
}

/**
 * The config's `claude-cli` entries, each a Claude Code account the server
 * runs Claude Code with. A server without entries, or that doesn't list
 * them, gets no card.
 */
export function ClaudeCliEntries() {
  const query = useApiQuery<Entries>(CLAUDE_CLI_ENTRIES, undefined, {
    refetchInterval: pollWhileRead,
  });
  const [notice, setNotice] = useState<ReactNode>(null);
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
      description={
        <>
          The claude-cli entries in config.yaml: each runs Claude Code, signed in with its own
          subscription. Add, change and delete them in config.yaml.
          <CheckedAt at={query.dataUpdatedAt} />
        </>
      }
    >
      {notice !== null && (
        <Alert tone="ok" live>
          <p>{notice}</p>
        </Alert>
      )}
      <PolledState query={query}>
        {(data) => (
          <TriageList
            entries={data.entries.map(entryOf)}
            render={(entry, view) => (
              <EntryItem
                entry={entry.entry}
                anchor={entry.anchor}
                compact={view.compact}
                targeted={view.targeted}
                onDone={setNotice}
              />
            )}
          />
        )}
      </PolledState>
    </Card>
  );
}
