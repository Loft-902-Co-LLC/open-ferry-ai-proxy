import type { ReactNode } from "react";

import type { CallProblem } from "../api/access";
import { Alert, type AlertTone } from "./Alert";
import { Code } from "./Code";

interface Explanation {
  tone: AlertTone;
  title: string;
  body: ReactNode;
}

/** What a failed call means and what to do about it, in plain words. */
export function explainProblem(problem: CallProblem): Explanation {
  switch (problem.kind) {
    case "unreachable":
      return {
        tone: "danger",
        title: "The server didn't answer",
        body: (
          <p>
            Check that open-ferry is running and that this address reaches it. If a proxy sits in
            front of it, check that the proxy passes requests on.
          </p>
        ),
      };
    case "missing-key":
      return {
        tone: "danger",
        title: "No management key was sent",
        body: <p>Enter the management key to sign in.</p>,
      };
    case "wrong-key":
      return {
        tone: "danger",
        title: "That management key is wrong",
        body: (
          <>
            <p>
              Enter the value of <Code>remote-management.secret-key</Code> from config.yaml as you
              wrote it, or of the <Code>MANAGEMENT_PASSWORD</Code> environment variable. If
              config.yaml now shows a hash starting with <Code>$2a$</Code>, the server hashed your
              key: enter the key itself, not the hash.
            </p>
            <p>
              The local password, from <Code>-password</Code> or the terminal UI&apos;s standalone
              mode, is taken only from the computer the server runs on.
            </p>
            <p>Five wrong keys from one address lock that address out for thirty minutes.</p>
          </>
        ),
      };
    case "remote-disabled":
      return {
        tone: "danger",
        title: "Remote management is off",
        body: (
          <p>
            This server takes management calls only from the computer it runs on. Open the
            dashboard on that computer, through <Code>127.0.0.1</Code> or <Code>localhost</Code>.
            To manage it from elsewhere, set <Code>remote-management.allow-remote: true</Code> in
            config.yaml; setting <Code>MANAGEMENT_PASSWORD</Code> allows it too.
          </p>
        ),
      };
    case "banned":
      return {
        tone: "danger",
        title: "This address is locked out",
        body: (
          <p>
            Too many wrong keys came from this address, so the server refuses it for thirty
            minutes.{" "}
            {problem.retryIn === null
              ? "Try again later."
              : `Try again in ${problem.retryIn}.`}
          </p>
        ),
      };
    case "management-off":
      return {
        tone: "warn",
        title: "Management is switched off on this server",
        body: (
          <p>
            No management key is set, so the server answers no management calls and the dashboard
            can't work. Set <Code>remote-management.secret-key</Code> in config.yaml, or start the
            server with the <Code>MANAGEMENT_PASSWORD</Code> environment variable set, then try
            again. A local password, from <Code>-password</Code> or the terminal UI&apos;s
            standalone mode, doesn&apos;t turn management on by itself.
          </p>
        ),
      };
    case "ledger-unavailable":
      return {
        tone: "danger",
        title: "The usage ledger couldn't be opened",
        body: (
          <p>
            The server keeps usage in a file in its log directory, and it couldn't open that file.
            The ledger section on the Usage page says why.
          </p>
        ),
      };
    case "unsupported":
      return {
        tone: "info",
        title: "This server can't do that yet",
        body: <p>This version of open-ferry doesn't serve this part of the management API.</p>,
      };
    case "not-found":
      return {
        tone: "warn",
        title: "Not found",
        body: <p>The server has no such item. It may have been removed since this page loaded.</p>,
      };
    case "invalid":
      return {
        tone: "danger",
        title: "The server refused the request",
        body: <p>{problem.message ?? "It didn't say why."}</p>,
      };
    case "server-error":
      return {
        tone: "danger",
        title: `The server failed (HTTP ${String(problem.status)})`,
        body: (
          <p>
            {problem.message ?? "It didn't say why."} The server's log may say more. Try again in a
            moment.
          </p>
        ),
      };
    case "unexpected":
      return {
        tone: "danger",
        title:
          problem.status === 0
            ? "Something went wrong"
            : `Unexpected answer (HTTP ${String(problem.status)})`,
        body: <p>{problem.message ?? "The server didn't say why."}</p>,
      };
  }
}

export interface ProblemNoticeProps {
  problem: CallProblem;
  /** Announce it as it appears: for errors that follow an action. */
  live?: boolean;
  /** Something to do about it, such as a retry button. */
  action?: ReactNode;
  className?: string;
}

export function ProblemNotice({ problem, live = false, action, className }: ProblemNoticeProps) {
  const { tone, title, body } = explainProblem(problem);
  return (
    <Alert tone={tone} title={title} live={live} className={className}>
      {body}
      {action !== undefined && <div className="flex flex-wrap gap-2">{action}</div>}
    </Alert>
  );
}
