import { KeyRound, LogIn, Upload } from "lucide-react";
import { Link } from "react-router";

import { callProblem } from "../../api/access";
import { isUnsupportedRoute } from "../../api/client";
import { AUTH_FILES, KEY_LISTS, keysOf, type CredentialList } from "../../api/credentials";
import { CLAUDE_CLI_ENTRIES, type ClaudeCliEntries } from "../../api/dashboard";
import { useApiQuery } from "../../api/hooks";
import { Badge, type BadgeTone } from "../../components/Badge";
import { buttonClasses } from "../../components/Button";
import { Card } from "../../components/Card";
import { ProblemNotice } from "../../components/ProblemNotice";
import { Loading } from "../../components/QueryState";
import { formatInteger } from "../../lib/format";
import { entriesUnserved, entryHealth } from "../credentials/claudeCli";
import { credentialHealth, type Health } from "../credentials/credentialStates";

function count(value: number, one: string, many: string): string {
  return `${formatInteger(value)} ${value === 1 ? one : many}`;
}

/** The ways to connect a provider, each opening its dialog on Credentials. */
function ConnectLinks() {
  return (
    <div className="flex flex-wrap gap-2">
      <Link to="/credentials?start=claude" className={buttonClasses("primary")}>
        <LogIn aria-hidden="true" className="size-4" />
        Sign in with Claude
      </Link>
      <Link to="/credentials?start=codex" className={buttonClasses("secondary")}>
        <LogIn aria-hidden="true" className="size-4" />
        Sign in with Codex
      </Link>
      <Link to="/credentials?start=key" className={buttonClasses("secondary")}>
        <KeyRound aria-hidden="true" className="size-4" />
        Add a provider API key
      </Link>
      <Link to="/credentials" className={buttonClasses("secondary")}>
        <Upload aria-hidden="true" className="size-4" />
        Upload a credential file
      </Link>
    </div>
  );
}

/**
 * The providers the server can send requests to: on a first run, the ways
 * to connect one; after that, how its credentials are doing.
 */
export function ProvidersCard() {
  const files = useApiQuery<CredentialList>(AUTH_FILES);
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
  const accounts = useApiQuery<ClaudeCliEntries>(CLAUDE_CLI_ENTRIES);
  const accountsUnserved = accounts.isError && entriesUnserved(accounts.error);

  // A server that serves none of it gets no card: there is nothing to do here.
  if (queries.every((query) => isUnsupportedRoute(query.error)) && accountsUnserved) {
    return null;
  }
  if (queries.some((query) => query.isPending) || accounts.isPending) {
    return (
      <Card title="Providers">
        <Loading>Loading the credentials…</Loading>
      </Card>
    );
  }

  const openLink = (
    <Link to="/credentials" className={buttonClasses("secondary", "sm")}>
      Open Credentials
    </Link>
  );
  const failed =
    queries.find((query) => query.isError && !isUnsupportedRoute(query.error)) ??
    (accounts.isError && !accountsUnserved ? accounts : undefined);
  if (failed !== undefined) {
    return (
      <Card title="Providers" actions={openLink}>
        <ProblemNotice problem={callProblem(failed.error)} />
      </Card>
    );
  }

  const credentials = files.data?.files ?? [];
  const keys = keyQueries.reduce(
    (sum, { query, list }) => sum + (query.isSuccess ? keysOf(query.data, list).length : 0),
    0,
  );
  const entries = accounts.isSuccess ? accounts.data.entries : [];

  if (credentials.length === 0 && keys === 0 && entries.length === 0) {
    return (
      <Card
        title="Connect a provider"
        description="The server has no account or key to send requests with yet."
      >
        <p>
          Sign in with a Claude or ChatGPT account, add an API key from a provider&apos;s console,
          or upload a credential file another open-ferry or CLIProxyAPI saved.
        </p>
        <ConnectLinks />
      </Card>
    );
  }

  const tally = new Map<string, { tone: BadgeTone; count: number }>();
  const healths: Health[] = [
    ...credentials.map((credential) => credentialHealth(credential)),
    ...entries.map((entry) => entryHealth(entry)),
  ];
  for (const { label, tone } of healths) {
    const entry = tally.get(label) ?? { tone, count: 0 };
    entry.count += 1;
    tally.set(label, entry);
  }
  const attention = (tally.get("Failing")?.count ?? 0) + (tally.get("Resting")?.count ?? 0);

  return (
    <Card
      title="Providers"
      description={`${count(credentials.length, "sign-in or credential file", "sign-ins and credential files")}, ${count(keys, "provider API key", "provider API keys")}${entries.length === 0 ? "" : `, ${count(entries.length, "Claude Code account", "Claude Code accounts")}`}.`}
      actions={openLink}
    >
      {tally.size > 0 && (
        <ul aria-label="Credential health" className="flex flex-wrap gap-2">
          {[...tally].map(([label, { tone, count: value }]) => (
            <li key={label}>
              <Badge tone={tone}>
                {formatInteger(value)} {label.toLowerCase()}
              </Badge>
            </li>
          ))}
        </ul>
      )}
      {attention > 0 && (
        <p>
          {attention === 1 ? "One needs" : `${formatInteger(attention)} need`} attention: the
          Credentials page says why, and what to do.
        </p>
      )}
    </Card>
  );
}
