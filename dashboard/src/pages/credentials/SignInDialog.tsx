import { zodResolver } from "@hookform/resolvers/zod";
import { useMutation, useQueryClient, type UseQueryResult } from "@tanstack/react-query";
import { ExternalLink, Eye, EyeOff, LogIn, RotateCw } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { useForm } from "react-hook-form";

import { callProblem } from "../../api/access";
import { isApiError } from "../../api/client";
import {
  AUTH_FILES,
  BIND_CALLBACK,
  BIND_SESSION,
  BIND_START,
  BIND_STATUS,
  SIGN_IN_CALLBACK,
  SIGN_IN_SESSION,
  SIGN_IN_START,
  SIGN_IN_STATUS,
  type CancelAnswer,
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
  explainBindError,
  explainBindStartError,
  explainSignInError,
  explainStartError,
  pastedAddressProblem,
  SIGN_IN_TIMED_OUT,
  signInName,
  type ReasonText,
} from "./credentialStates";

/** How often a sign-in's status is asked for while it waits. */
export const SIGN_IN_POLL_MS = 2000;

/** How long the server waits for a sign-in to finish before it stops. */
export const SIGN_IN_LIMIT_MS = 5 * 60_000;

/** From this many seconds left, the dialog says time is nearly up. */
const LAST_MINUTE_S = 60;

/** A sign-in the server has started. */
interface Session {
  url: string;
  state: string;
  /** Started without the local callback: only a pasted address finishes it. */
  pasteOnly: boolean;
  /** When it was asked for: the server's five minutes start just after. */
  startedAt: number;
}

/**
 * The whole seconds left of a sign-in asked for at `startedAt`, counted down
 * once a second; null when no sign-in is waiting.
 */
function useSecondsLeft(startedAt: number | null): number | null {
  const [now, setNow] = useState(0);
  useEffect(() => {
    if (startedAt === null) {
      return undefined;
    }
    const timer = setInterval(() => {
      const at = Date.now();
      setNow(at);
      if (at >= startedAt + SIGN_IN_LIMIT_MS) {
        clearInterval(timer);
      }
    }, 1000);
    return () => {
      clearInterval(timer);
    };
  }, [startedAt]);
  if (startedAt === null) {
    return null;
  }
  const left = startedAt + SIGN_IN_LIMIT_MS - Math.max(now, startedAt);
  return Math.max(0, Math.ceil(left / 1000));
}

/** "4:05" */
function minutesAndSeconds(seconds: number): string {
  return `${String(Math.floor(seconds / 60))}:${String(seconds % 60).padStart(2, "0")}`;
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
function PasteFailure({ error, bind }: { error: unknown; bind: boolean }) {
  if (
    isApiError(error) &&
    error.code !== null &&
    (error.status === 400 || error.status === 404 || error.status === 409)
  ) {
    return <Failure reason={bind ? explainBindError(error.code) : explainSignInError(error.code)} />;
  }
  return <ProblemNotice problem={callProblem(error)} live />;
}

/**
 * Where to paste the address the provider sent the browser to. The address
 * shows, so you can check what you pasted: its one-time code works only for
 * this sign-in, and only with what the server keeps. It is never logged.
 */
function PasteForm({
  provider,
  callback,
  state,
  onSent,
}: {
  provider: SignInProvider;
  /** The route that takes the address. */
  callback: string;
  state: string;
  onSent: () => void;
}) {
  const call = useApiCall();
  const [shown, setShown] = useState(true);
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
      call<unknown>(callback, {
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
        type={shown ? "text" : "password"}
        autoComplete="off"
        spellCheck={false}
        autoCapitalize="none"
        placeholder="http://localhost:…"
        error={form.formState.errors.address?.message}
        {...form.register("address")}
      />
      <div className="flex flex-wrap items-center gap-2">
        <Button type="submit" size="sm" disabled={send.isPending}>
          {send.isPending ? <Spinner /> : <LogIn aria-hidden="true" className="size-4" />}
          Finish signing in
        </Button>
        <Button
          size="sm"
          variant="ghost"
          onClick={() => {
            setShown(!shown);
          }}
        >
          {shown ? (
            <EyeOff aria-hidden="true" className="size-4" />
          ) : (
            <Eye aria-hidden="true" className="size-4" />
          )}
          {shown ? "Hide the address" : "Show the address"}
        </Button>
      </div>
      {send.isSuccess && (
        <p role="status" className="text-muted">
          Sent: the server is finishing the sign-in.
        </p>
      )}
      {send.isError && <PasteFailure error={send.error} bind={callback === BIND_CALLBACK} />}
    </form>
  );
}

/** Following a started sign-in until it ends. */
function Waiting({
  provider,
  callback,
  session,
  status,
  secondsLeft,
  onPasted,
}: {
  provider: SignInProvider;
  callback: string;
  session: Session;
  status: UseQueryResult<SignInStatus>;
  secondsLeft: number;
  onPasted: () => void;
}) {
  const name = signInName(provider);
  const link = useRef<HTMLAnchorElement>(null);
  useEffect(() => {
    link.current?.focus();
  }, [session.state]);
  const lastMinute = secondsLeft <= LAST_MINUTE_S;

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
          <PasteForm
            provider={provider}
            callback={callback}
            state={session.state}
            onSent={onPasted}
          />
        </li>
      </ol>
      {lastMinute && (
        <Alert tone="warn" title="Time is nearly up">
          <p>
            When the time left reaches 0:00, the server stops waiting. If you need longer, start
            again: that gives you another five minutes.
          </p>
        </Alert>
      )}
      <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-1 text-muted">
        <p role="status" className="flex items-center gap-2">
          <Spinner /> Waiting for you to sign in…
        </p>
        {/* Not a live region: it changes every second. */}
        <p className="tabular-nums">Time left: {minutesAndSeconds(secondsLeft)}</p>
      </div>
      <p role="status" className="sr-only">
        {lastMinute ? "One minute left to finish signing in. Start again if you need longer." : ""}
      </p>
      {status.isError && <ProblemNotice problem={callProblem(status.error)} />}
    </div>
  );
}

/** The credential file a sign-in binds Sign in with ChatGPT to, and its shown name. */
export interface BindTarget {
  file: string;
  name: string;
}

export interface SignInDialogProps {
  provider: SignInProvider;
  /**
   * Binds Sign in with ChatGPT to this existing Codex credential instead of
   * making a new one. Only for `provider="codex"`.
   */
  bind?: BindTarget;
  onClose: () => void;
}

/**
 * Signing in with Claude or ChatGPT (for Codex): starts the server's sign-in,
 * links to the provider's page, follows it with the time left, and takes the
 * address the provider sent the browser to when that can't reach the server.
 * Nothing starts until asked: opening the dialog from a link has no effect on
 * the server.
 *
 * With `bind`, the same steps add Sign in with ChatGPT to a credential that
 * is already there. A bind the server was already saving when it was given
 * up can't be cancelled: the dialog keeps following it to its end.
 */
export function SignInDialog({ provider, bind, onClose }: SignInDialogProps) {
  const name = signInName(provider);
  const account = provider === "codex" ? "your ChatGPT account (for Codex)" : name;
  const call = useApiCall();
  const client = useQueryClient();
  const [session, setSession] = useState<Session | null>(null);
  // The server was already saving the bind when it was given up: it finishes.
  const [finishing, setFinishing] = useState(false);
  // A give-up has been sent and its answer isn't back.
  const [cancelling, setCancelling] = useState(false);
  // Set once the dialog closes: a sign-in that starts after that is given up.
  const closed = useRef(false);
  const content = useRef<HTMLDivElement>(null);
  const routes =
    bind === undefined
      ? {
          start: SIGN_IN_START[provider],
          status: SIGN_IN_STATUS,
          callback: SIGN_IN_CALLBACK,
          session: SIGN_IN_SESSION,
        }
      : {
          start: BIND_START,
          status: BIND_STATUS,
          callback: BIND_CALLBACK,
          session: BIND_SESSION,
        };

  const giveUp = (state: string) => {
    call<unknown>(routes.session, { method: "DELETE", query: { state } }).catch(() => undefined);
  };

  /** Gives `state` up; whether the server stopped it (a bind it is saving isn't stopped). */
  const cancelBind = async (state: string): Promise<boolean> => {
    try {
      const answer = await call<CancelAnswer>(routes.session, {
        method: "DELETE",
        query: { state },
      });
      return answer.cancelled !== false;
    } catch {
      return true;
    }
  };

  const start = useMutation({
    mutationFn: async (pasteOnly: boolean): Promise<Session> => {
      const startedAt = Date.now();
      const answer = await call<SignInStart>(routes.start, {
        query:
          bind === undefined
            ? pasteOnly
              ? undefined
              : { is_webui: true }
            : {
                provider: "codex",
                mode: "siwc-bind",
                name: bind.file,
                is_webui: pasteOnly ? undefined : 1,
              },
      });
      return { url: answer.url, state: answer.state, pasteOnly, startedAt };
    },
    onMutate: () => {
      setSession(null);
      setFinishing(false);
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
    routes.status,
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
  // The server hasn't said how it ended: it may still be waiting.
  const stillWaiting = session !== null && outcome !== "ok" && outcome !== "error";
  // A bind being saved has no time limit left to count.
  const secondsLeft = useSecondsLeft(stillWaiting && !finishing ? session.startedAt : null);
  // The five minutes ran out here; the server says so too, a moment later,
  // with the same words, so nothing is said twice.
  const timedOut = stillWaiting && !finishing && secondsLeft === 0;

  useEffect(() => {
    if (outcome === "ok") {
      void client.invalidateQueries({ queryKey: [AUTH_FILES] });
    }
  }, [outcome, client]);

  // Each step names the control to start from; the waiting step focuses
  // its link itself.
  const step =
    session === null
      ? start.isError
        ? "start-failed"
        : "start"
      : outcome === "ok"
        ? "ok"
        : outcome === "error" || timedOut
          ? "error"
          : finishing
            ? "finishing"
            : "waiting";
  useEffect(() => {
    if (step === "start-failed" || step === "ok" || step === "error" || step === "finishing") {
      content.current
        ?.closest("dialog")
        ?.querySelector<HTMLElement>("[data-autofocus]")
        ?.focus();
    }
  }, [step]);

  /**
   * Gives up a sign-in still waiting, so the server stops waiting, then goes
   * on with `proceed`. A bind the server is already saving can't be given
   * up: the dialog follows it to its end instead.
   */
  const giveUpThen = (proceed: () => void) => {
    if (session === null || !stillWaiting || finishing) {
      proceed();
      return;
    }
    if (bind === undefined) {
      giveUp(session.state);
      proceed();
      return;
    }
    setCancelling(true);
    void cancelBind(session.state).then((cancelled) => {
      setCancelling(false);
      if (cancelled) {
        proceed();
      } else {
        setFinishing(true);
      }
    });
  };

  const close = () => {
    giveUpThen(() => {
      closed.current = true;
      onClose();
    });
    // Closed while it saves: the list shows the result once it is read again.
    if (finishing) {
      void client.invalidateQueries({ queryKey: [AUTH_FILES] });
    }
  };

  const startAgain = () => {
    giveUpThen(() => {
      start.mutate(session?.pasteOnly ?? false);
    });
  };

  const startProblem =
    start.isError && isApiError(start.error)
      ? bind === undefined
        ? explainStartError(provider, start.error.status, start.error.code)
        : explainBindStartError(start.error.status, start.error.code)
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
    ) : bind === undefined ? (
      <p>
        The server starts a sign-in with {account}. You sign in on {name}&apos;s site in a new tab,
        and the server saves the credential it gets, which then shows in the list.
      </p>
    ) : (
      <div className="space-y-2">
        <p>
          <span className="font-medium break-all">{bind.name}</span> keeps its Codex sign-in and
          gains Sign in with ChatGPT tokens. Its requests then go to the OpenAI API, under your
          ChatGPT plan, instead of ChatGPT&apos;s Codex backend.
        </p>
        <p>
          You sign in on {name}&apos;s site in a new tab, and the server saves the tokens to this
          credential&apos;s file.
        </p>
      </div>
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
  } else if (step === "ok") {
    body =
      bind === undefined ? (
        <Alert tone="ok" live title={`Signed in with ${name}`}>
          <p>The server saved the new credential: it is in the list now.</p>
        </Alert>
      ) : (
        <Alert tone="ok" live title="OpenAI API access added">
          <p>
            The server saved Sign in with ChatGPT tokens to{" "}
            <span className="font-medium break-all">{bind.name}</span>. Its requests now go to the
            OpenAI API.
          </p>
        </Alert>
      );
    footer = (
      <Button variant="primary" data-autofocus onClick={close}>
        Done
      </Button>
    );
  } else if (step === "error") {
    const error = outcome === "error" ? (status.data?.error ?? "") : SIGN_IN_TIMED_OUT;
    body = (
      <Failure reason={bind === undefined ? explainSignInError(error) : explainBindError(error)} />
    );
    footer = (
      <>
        <Button onClick={close}>Close</Button>
        <Button variant="primary" data-autofocus onClick={startAgain}>
          <RotateCw aria-hidden="true" className="size-4" />
          Start again
        </Button>
      </>
    );
  } else if (step === "finishing") {
    body = (
      <div className="space-y-3">
        <Alert tone="info" live title="The server is already saving it">
          <p>
            It had your sign-in and was saving the access when you gave up, so it can&apos;t stop
            now. It finishes on its own.
          </p>
        </Alert>
        <p role="status" className="flex items-center gap-2 text-muted">
          <Spinner /> Waiting for the server to finish saving…
        </p>
        {status.isError && <ProblemNotice problem={callProblem(status.error)} />}
      </div>
    );
    footer = (
      <Button variant="primary" data-autofocus onClick={close}>
        Close
      </Button>
    );
  } else {
    const left = secondsLeft ?? Math.ceil(SIGN_IN_LIMIT_MS / 1000);
    body = (
      <Waiting
        provider={provider}
        callback={routes.callback}
        session={session}
        status={status}
        secondsLeft={left}
        onPasted={() => {
          void status.refetch();
        }}
      />
    );
    footer = (
      <>
        <Button disabled={cancelling} onClick={close}>
          Give up
        </Button>
        {left <= LAST_MINUTE_S && (
          <Button disabled={cancelling} onClick={startAgain}>
            <RotateCw aria-hidden="true" className="size-4" />
            Start again
          </Button>
        )}
      </>
    );
  }

  return (
    <Dialog
      open
      title={bind === undefined ? `Sign in with ${name}` : "Add OpenAI API access"}
      onClose={close}
      footer={footer}
    >
      <div ref={content}>{body}</div>
    </Dialog>
  );
}
