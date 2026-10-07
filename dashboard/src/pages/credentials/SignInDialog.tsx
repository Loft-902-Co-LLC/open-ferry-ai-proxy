import { zodResolver } from "@hookform/resolvers/zod";
import { useMutation, useQueryClient, type UseQueryResult } from "@tanstack/react-query";
import { ExternalLink, LogIn, RotateCw } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { useForm } from "react-hook-form";

import { callProblem } from "../../api/access";
import { isApiError } from "../../api/client";
import {
  AUTH_FILES,
  SIGN_IN_CALLBACK,
  SIGN_IN_SESSION,
  SIGN_IN_START,
  SIGN_IN_STATUS,
  type SignInProvider,
  type SignInStart,
  type SignInStatus,
} from "../../api/credentials";
import { useApiCall, useApiQuery } from "../../api/hooks";
import { Alert } from "../../components/Alert";
import { Button, buttonClasses } from "../../components/Button";
import { CopyButton } from "../../components/CopyButton";
import { Dialog } from "../../components/Dialog";
import { ProblemNotice } from "../../components/ProblemNotice";
import { Spinner } from "../../components/Spinner";
import { TextField } from "../../components/TextField";
import { z } from "../../lib/zod";
import {
  explainSignInError,
  explainStartError,
  pastedAddressProblem,
  providerName,
  type ReasonText,
} from "./credentialStates";

/** How often a sign-in's status is asked for while it waits. */
export const SIGN_IN_POLL_MS = 2000;

/** A sign-in the server has started. */
interface Session {
  url: string;
  state: string;
  /** Started without the local callback: only a pasted address finishes it. */
  pasteOnly: boolean;
}

/** A sign-in failure, explained. */
function Failure({ reason }: { reason: ReasonText }) {
  return (
    <Alert tone="danger" live title={reason.title}>
      <p>{reason.meaning}</p>
      <p>
        <span className="font-medium">What to do:</span> {reason.action}
      </p>
    </Alert>
  );
}

/** Why a pasted address was refused, explained where the server said why. */
function PasteFailure({ error }: { error: unknown }) {
  if (
    isApiError(error) &&
    error.code !== null &&
    (error.status === 400 || error.status === 404 || error.status === 409)
  ) {
    return <Failure reason={explainSignInError(error.code)} />;
  }
  return <ProblemNotice problem={callProblem(error)} live />;
}

/** Where to paste the address the provider sent the browser to. */
function PasteForm({
  provider,
  state,
  onSent,
}: {
  provider: SignInProvider;
  state: string;
  onSent: () => void;
}) {
  const call = useApiCall();
  const schema = useMemo(
    () =>
      z.object({
        address: z.string().superRefine((value, context) => {
          const problem =
            value.trim() === ""
              ? "Paste the address of the page the provider sent you to."
              : pastedAddressProblem(value, state);
          if (problem !== null) {
            context.addIssue({ code: "custom", message: problem });
          }
        }),
      }),
    [state],
  );
  const form = useForm<{ address: string }>({
    resolver: zodResolver(schema),
    defaultValues: { address: "" },
  });
  const send = useMutation({
    mutationFn: (address: string) =>
      call<unknown>(SIGN_IN_CALLBACK, {
        method: "POST",
        json: { provider, redirect_url: address },
      }),
    onSuccess: () => {
      form.reset();
      onSent();
    },
  });

  return (
    <form
      noValidate
      className="space-y-2"
      onSubmit={(event) => {
        void form.handleSubmit(({ address }) => {
          send.mutate(address.trim());
        })(event);
      }}
    >
      <TextField
        label="Address of the page it sent you to"
        secret
        revealLabel="Show the address"
        placeholder="http://localhost:…"
        error={form.formState.errors.address?.message}
        {...form.register("address")}
      />
      <Button type="submit" size="sm" disabled={send.isPending}>
        {send.isPending ? <Spinner /> : <LogIn aria-hidden="true" className="size-4" />}
        Finish signing in
      </Button>
      {send.isSuccess && (
        <p role="status" className="text-muted">
          Sent: the server is finishing the sign-in.
        </p>
      )}
      {send.isError && <PasteFailure error={send.error} />}
    </form>
  );
}

/** Following a started sign-in until it ends. */
function Waiting({
  provider,
  session,
  status,
  onPasted,
}: {
  provider: SignInProvider;
  session: Session;
  status: UseQueryResult<SignInStatus>;
  onPasted: () => void;
}) {
  const name = providerName(provider);
  const link = useRef<HTMLAnchorElement>(null);
  useEffect(() => {
    link.current?.focus();
  }, [session.state]);

  return (
    <div className="space-y-4">
      <ol className="list-decimal space-y-4 pl-5">
        <li className="space-y-2">
          <p>
            Open {name}&apos;s sign-in page, and sign in there. Finish within five minutes of
            starting: after that the server stops waiting, and you start again.
          </p>
          <div className="flex flex-wrap items-center gap-2">
            <a
              ref={link}
              href={session.url}
              target="_blank"
              rel="noopener noreferrer"
              className={buttonClasses("primary", "sm")}
            >
              Open {name}&apos;s sign-in page
              <span className="sr-only"> (opens in a new tab)</span>
              <ExternalLink aria-hidden="true" className="size-4" />
            </a>
            <CopyButton text={session.url} label="Copy the sign-in link" />
          </div>
        </li>
        <li className="space-y-2">
          <p>
            {session.pasteOnly
              ? `${name} then sends you to a page on localhost that won't load. Copy its whole address from the address bar, and paste it here.`
              : `${name} then sends you to a page on localhost. If this browser runs on the server's computer, the sign-in finishes on its own. If the page doesn't load, copy its whole address from the address bar, and paste it here.`}
          </p>
          <PasteForm provider={provider} state={session.state} onSent={onPasted} />
        </li>
      </ol>
      <p role="status" className="flex items-center gap-2 text-muted">
        <Spinner /> Waiting for you to sign in…
      </p>
      {status.isError && <ProblemNotice problem={callProblem(status.error)} />}
    </div>
  );
}

export interface SignInDialogProps {
  provider: SignInProvider;
  onClose: () => void;
}

/**
 * Signing in with Claude or Codex: starts the server's sign-in, links to the
 * provider's page, follows it, and takes the address the provider sent the
 * browser to when that can't reach the server. Nothing starts until asked:
 * opening the dialog from a link has no effect on the server.
 */
export function SignInDialog({ provider, onClose }: SignInDialogProps) {
  const name = providerName(provider);
  const call = useApiCall();
  const client = useQueryClient();
  const [session, setSession] = useState<Session | null>(null);
  // Set once the dialog closes: a sign-in that starts after that is given up.
  const closed = useRef(false);
  const content = useRef<HTMLDivElement>(null);

  const giveUp = (state: string) => {
    call<unknown>(SIGN_IN_SESSION, { method: "DELETE", query: { state } }).catch(() => undefined);
  };

  const start = useMutation({
    mutationFn: async (pasteOnly: boolean): Promise<Session> => {
      const answer = await call<SignInStart>(SIGN_IN_START[provider], {
        query: pasteOnly ? undefined : { is_webui: true },
      });
      return { url: answer.url, state: answer.state, pasteOnly };
    },
    onMutate: () => {
      setSession(null);
    },
    onSuccess: (started) => {
      if (closed.current) {
        giveUp(started.state);
        return;
      }
      setSession(started);
    },
  });

  const status = useApiQuery<SignInStatus>(
    SIGN_IN_STATUS,
    { state: session?.state ?? "" },
    {
      enabled: session !== null,
      gcTime: 0,
      refetchInterval: (query) => {
        const current = query.state.data?.status;
        return current === "ok" || current === "error" ? false : SIGN_IN_POLL_MS;
      },
    },
  );
  const outcome = session === null ? undefined : status.data?.status;

  useEffect(() => {
    if (outcome === "ok") {
      void client.invalidateQueries({ queryKey: [AUTH_FILES] });
    }
  }, [outcome, client]);

  // Each step names the control to start from; the waiting step focuses
  // its link itself.
  const step =
    session === null ? (start.isError ? "start-failed" : "start") : (outcome ?? "waiting");
  useEffect(() => {
    if (step === "start-failed" || step === "ok" || step === "error") {
      content.current
        ?.closest("dialog")
        ?.querySelector<HTMLElement>("[data-autofocus]")
        ?.focus();
    }
  }, [step]);

  const close = () => {
    closed.current = true;
    // A sign-in still waiting is given up, so the server stops waiting.
    if (session !== null && outcome !== "ok" && outcome !== "error") {
      giveUp(session.state);
    }
    onClose();
  };

  const startProblem =
    start.isError && isApiError(start.error)
      ? explainStartError(provider, start.error.status, start.error.code)
      : null;

  let body;
  let footer;
  if (session === null) {
    body = start.isPending ? (
      <p role="status" className="flex items-center gap-2 text-muted">
        <Spinner /> Starting the sign-in…
      </p>
    ) : start.isError ? (
      startProblem === null ? (
        <ProblemNotice problem={callProblem(start.error)} live />
      ) : (
        <Failure reason={startProblem.reason} />
      )
    ) : (
      <p>
        The server starts a sign-in with {name}. You sign in on {name}&apos;s site in a new tab, and
        the server saves the credential it gets, which then shows in the list.
      </p>
    );
    footer = (
      <>
        <Button onClick={close}>Cancel</Button>
        {startProblem?.pasteOnly === true && (
          <Button
            variant="primary"
            data-autofocus
            onClick={() => {
              start.mutate(true);
            }}
          >
            Start without it
          </Button>
        )}
        {startProblem?.pasteOnly !== true && (
          <Button
            variant="primary"
            data-autofocus
            disabled={start.isPending}
            onClick={() => {
              start.mutate(false);
            }}
          >
            {start.isError ? (
              <RotateCw aria-hidden="true" className="size-4" />
            ) : (
              <LogIn aria-hidden="true" className="size-4" />
            )}
            {start.isError ? "Try again" : "Start"}
          </Button>
        )}
      </>
    );
  } else if (outcome === "ok") {
    body = (
      <Alert tone="ok" live title={`Signed in with ${name}`}>
        <p>The server saved the new credential: it is in the list now.</p>
      </Alert>
    );
    footer = (
      <Button variant="primary" data-autofocus onClick={close}>
        Done
      </Button>
    );
  } else if (outcome === "error") {
    body = <Failure reason={explainSignInError(status.data?.error ?? "")} />;
    footer = (
      <>
        <Button onClick={close}>Close</Button>
        <Button
          variant="primary"
          data-autofocus
          onClick={() => {
            start.mutate(session.pasteOnly);
          }}
        >
          <RotateCw aria-hidden="true" className="size-4" />
          Start again
        </Button>
      </>
    );
  } else {
    body = (
      <Waiting
        provider={provider}
        session={session}
        status={status}
        onPasted={() => {
          void status.refetch();
        }}
      />
    );
    footer = <Button onClick={close}>Give up</Button>;
  }

  return (
    <Dialog open title={`Sign in with ${name}`} onClose={close} footer={footer}>
      <div ref={content}>{body}</div>
    </Dialog>
  );
}
