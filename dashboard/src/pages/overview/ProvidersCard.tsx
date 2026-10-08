import { KeyRound, LogIn, RotateCw } from "lucide-react";
import { useEffect, useState, type ReactNode } from "react";
import { Link } from "react-router";

import { callProblem } from "../../api/access";
import { isUnsupportedRoute } from "../../api/client";
import {
  AUTH_FILES,
  KEY_LISTS,
  keysOf,
  type Credential,
  type CredentialList,
} from "../../api/credentials";
import { CLAUDE_CLI_ENTRIES, type ClaudeCliEntries, type ClaudeCliEntry } from "../../api/dashboard";
import { useApiQuery } from "../../api/hooks";
import { Badge } from "../../components/Badge";
import { Button, buttonClasses } from "../../components/Button";
import { Card } from "../../components/Card";
import { ProblemNotice } from "../../components/ProblemNotice";
import { Loading } from "../../components/QueryState";
import { formatInteger, formatShortDateTime } from "../../lib/format";
import { claudeCliAnchor, credentialAnchor } from "../credentials/anchors";
import { entriesUnserved, entryHealth } from "../credentials/claudeCli";
import { clockMinutes } from "../credentials/clock";
import { credentialNames } from "../credentials/CredentialItem";
import { pollWhileRead } from "../credentials/CredentialList";
import { CheckedAt } from "../credentials/PolledState";
import {
  compareHealth,
  credentialHealth,
  healthTally,
  needsAttention,
  providerName,
  secondsUntilBack,
  signInName,
  timeLeft,
  type HealthOrder,
} from "../credentials/credentialStates";
import { useUpload } from "../credentials/upload";

/** Past this, a rest's end shows with its date, not just the time of day. */
const SAME_DAY_MS = 20 * 60 * 60_000;

function count(value: number, one: string, many: string): string {
  return `${formatInteger(value)} ${value === 1 ? one : many}`;
}

/** The time now, read again once a minute: for "in about 12 min". */
function useMinuteClock(): number {
  const [now, setNow] = useState(0);
  useEffect(() => {
    const timer = setInterval(() => {
      setNow(Date.now());
    }, 60_000);
    return () => {
      clearInterval(timer);
    };
  }, []);
  return now;
}

/** An account on the board: a sign-in, a credential file or a claude-cli entry. */
interface Account extends HealthOrder {
  key: string;
  /** Its element's id on the Credentials page. */
  anchor: string;
  provider: string;
  /** When its list was read, which `backIn` counts from. */
  readAt: number;
}

function credentialAccount(credential: Credential, name: string, readAt: number): Account {
  return {
    key: `file-${credential.id}`,
    anchor: credentialAnchor(credential.id),
    name,
    provider: providerName(credential.provider),
    health: credentialHealth(credential),
    backIn: secondsUntilBack(credential),
    readAt,
  };
}

function entryAccount(entry: ClaudeCliEntry, readAt: number): Account {
  return {
    key: `cli-${entry.name}`,
    anchor: claudeCliAnchor(entry.name),
    name: entry.name,
    provider: providerName("claude-cli"),
    health: entryHealth(entry),
    backIn: secondsUntilBack(entry.credential),
    readAt,
  };
}

/** "Back at 14:32 (in about 12 min)", as of `now`. */
function backAt(account: Account, now: number): string | null {
  if (account.health.triage !== "resting" || !Number.isFinite(account.backIn)) {
    return null;
  }
  const at = account.readAt + account.backIn * 1000;
  const left = Math.max(0, at - Math.max(now, account.readAt));
  const when =
    left < SAME_DAY_MS ? clockMinutes(at) : formatShortDateTime(new Date(at).toISOString());
  return `Back at ${when} (in ${timeLeft(left / 1000)})`;
}

/** One failing or resting account, linking to it on Credentials. */
function AttentionRow({ account, now }: { account: Account; now: number }) {
  const back = backAt(account, now);
  return (
    <li className="flex items-start justify-between gap-3 py-3 first:pt-0 last:pb-0">
      <div className="min-w-0 flex-1 space-y-0.5">
        <Link to={`/credentials#${account.anchor}`} className="font-medium break-all">
          {account.name}
        </Link>
        <p className="text-muted">
          {account.provider} · {account.health.summary}
        </p>
        {back !== null && <p className="text-muted">{back}</p>}
      </div>
      <Badge tone={account.health.tone} className="shrink-0">
        {account.health.label}
      </Badge>
    </li>
  );
}

/** What the board says about the accounts that need nothing, and the keys. */
function restLine(problems: number, rest: Account[], keys: number): string {
  const keyCount = count(keys, "provider API key", "provider API keys");
  if (problems === 0 && rest.length === 0) {
    return `The server has ${keyCount}. Keys don't report how they're doing, only accounts.`;
  }
  const also = keys === 0 ? "" : ` The server also has ${keyCount}.`;
  if (rest.length === 0) {
    return also.trim();
  }
  const tally = healthTally(rest.map((account) => account.health));
  if (problems > 0) {
    return `The rest: ${tally}.${also}`;
  }
  if (rest.every((account) => account.health.triage === "ready")) {
    return rest.length === 1
      ? `The one account is ready.${also}`
      : `All ${formatInteger(rest.length)} accounts are ready.${also}`;
  }
  return `Nothing needs attention: ${tally}.${also}`;
}

/** The ways to connect a provider: sign-ins and keys open their dialog on Credentials. */
function ConnectLinks({ upload }: { upload: ReactNode }) {
  return (
    <div className="flex flex-wrap gap-2">
      <Link to="/credentials?start=claude" className={buttonClasses("primary")}>
        <LogIn aria-hidden="true" className="size-4" />
        Sign in with Claude
      </Link>
      <Link to="/credentials?start=codex" className={buttonClasses("secondary")}>
        <LogIn aria-hidden="true" className="size-4" />
        Sign in with {signInName("codex")}
      </Link>
      <Link to="/credentials?start=key" className={buttonClasses("secondary")}>
        <KeyRound aria-hidden="true" className="size-4" />
        Add a provider API key
      </Link>
      {upload}
    </div>
  );
}

/**
 * How the server's accounts are doing: on a first run, the ways to connect
 * one; after that, the failing and resting ones, each linking to it on
 * Credentials, and a word on the rest.
 */
export function ProvidersCard() {
  const files = useApiQuery<CredentialList>(AUTH_FILES, undefined, {
    refetchInterval: pollWhileRead,
  });
  const claude = useApiQuery<unknown>(KEY_LISTS.claude.path);
  const codex = useApiQuery<unknown>(KEY_LISTS.codex.path);
  const gemini = useApiQuery<unknown>(KEY_LISTS.gemini.path);
  const keyQueries = [
    { query: claude, list: KEY_LISTS.claude.list },
    { query: codex, list: KEY_LISTS.codex.list },
    { query: gemini, list: KEY_LISTS.gemini.list },
  ];
  const queries = [files, claude, codex, gemini];
  // The config's claude-cli entries, which the credential list leaves out.
  const accounts = useApiQuery<ClaudeCliEntries>(CLAUDE_CLI_ENTRIES, undefined, {
    refetchInterval: pollWhileRead,
  });
  const accountsUnserved = accounts.isError && entriesUnserved(accounts.error);
  const upload = useUpload({ size: "md" });
  const clock = useMinuteClock();

  // A server that serves none of it gets no card: there is nothing to do here.
  if (queries.every((query) => isUnsupportedRoute(query.error)) && accountsUnserved) {
    return null;
  }
  if (queries.some((query) => query.isPending) || accounts.isPending) {
    return (
      <Card title="Account health">
        <Loading>Loading the credentials…</Loading>
      </Card>
    );
  }

  const openLink = (
    <Link to="/credentials" className={buttonClasses("secondary", "sm")}>
      Open Credentials
    </Link>
  );
  // A list read before keeps showing while a later read fails.
  const failed =
    queries.find(
      (query) => query.isError && query.data === undefined && !isUnsupportedRoute(query.error),
    ) ??
    (accounts.isError && accounts.data === undefined && !accountsUnserved ? accounts : undefined);
  if (failed !== undefined) {
    return (
      <Card title="Account health" actions={openLink}>
        <ProblemNotice
          problem={callProblem(failed.error)}
          action={
            <Button
              size="sm"
              onClick={() => {
                void failed.refetch();
              }}
            >
              <RotateCw aria-hidden="true" className="size-4" />
              Try again
            </Button>
          }
        />
      </Card>
    );
  }

  const credentials = files.data?.files ?? [];
  const keys = keyQueries.reduce(
    (sum, { query, list }) =>
      sum + (query.data === undefined ? 0 : keysOf(query.data, list).length),
    0,
  );
  const entries = accounts.data?.entries ?? [];

  if (credentials.length === 0 && keys === 0 && entries.length === 0) {
    return (
      <Card
        title="Connect a provider"
        description="The server has no account or key to send requests with yet."
      >
        {upload.result}
        <p>
          Sign in with a Claude account or a ChatGPT account (for Codex), add an API key from a
          provider&apos;s console, or upload credential files another open-ferry or CLIProxyAPI
          saved.
        </p>
        <ConnectLinks upload={upload.button} />
      </Card>
    );
  }

  const names = credentialNames(credentials);
  const board = [
    ...credentials.map((credential, index) =>
      credentialAccount(credential, names[index] ?? credential.name, files.dataUpdatedAt),
    ),
    ...entries.map((entry) => entryAccount(entry, accounts.dataUpdatedAt)),
  ].sort(compareHealth);
  const problems = board.filter((account) => needsAttention(account.health));
  const rest = board.filter((account) => !needsAttention(account.health));
  const now = Math.max(clock, files.dataUpdatedAt, accounts.dataUpdatedAt);
  const line = restLine(problems.length, rest, keys);
  // When the accounts on show were read: the older of the two lists polled.
  const reads = [files.dataUpdatedAt, accounts.dataUpdatedAt].filter((at) => at > 0);
  const checkedAt = reads.length === 0 ? 0 : Math.min(...reads);

  return (
    <Card
      title="Account health"
      description={
        <>
          The accounts that are failing or resting, and when they&apos;re back.
          <CheckedAt at={checkedAt} />
        </>
      }
      actions={openLink}
    >
      {upload.result}
      {problems.length > 0 && (
        <ul aria-label="Accounts that need attention" className="divide-y divide-line">
          {problems.map((account) => (
            <AttentionRow key={account.key} account={account} now={now} />
          ))}
        </ul>
      )}
      {line !== "" && <p className={problems.length > 0 ? "text-muted" : undefined}>{line}</p>}
    </Card>
  );
}
