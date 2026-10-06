import { zodResolver } from "@hookform/resolvers/zod";
import { useMutation } from "@tanstack/react-query";
import { KeyRound } from "lucide-react";
import { useForm } from "react-hook-form";
import { Navigate, useLocation } from "react-router";

import { checkManagementKey, signInProblem } from "../api/signIn";
import { asksForSafeModeSetup } from "../app/safeMode";
import type { ReturnTo } from "../app/routes";
import { Alert } from "../components/Alert";
import { Brand } from "../components/Brand";
import { Button } from "../components/Button";
import { Code } from "../components/Code";
import { usePageTitle } from "../components/PageHeader";
import { ProblemNotice } from "../components/ProblemNotice";
import { Spinner } from "../components/Spinner";
import { TextField } from "../components/TextField";
import { z } from "../lib/zod";
import { useSession } from "../session/session";

const schema = z.object({
  key: z.string().trim().min(1, "Enter the management key."),
});
type SignInForm = z.infer<typeof schema>;

/** Where to go once signed in: back where the visit was headed. */
function destination(state: unknown, search: string): ReturnTo {
  if (state !== null && typeof state === "object" && "from" in state) {
    const from = state.from;
    if (
      from !== null &&
      typeof from === "object" &&
      typeof (from as ReturnTo).pathname === "string" &&
      typeof (from as ReturnTo).search === "string" &&
      (from as ReturnTo).pathname !== "/signin"
    ) {
      return from as ReturnTo;
    }
  }
  return { pathname: "/", search: asksForSafeModeSetup(search) ? search : "" };
}

export function SignInPage() {
  const { key, signOutReason, signIn } = useSession();
  const location = useLocation();
  const to = destination(location.state, location.search);
  const safeMode = asksForSafeModeSetup(to.search);

  const form = useForm<SignInForm>({ resolver: zodResolver(schema), defaultValues: { key: "" } });
  const check = useMutation({
    mutationFn: (candidate: string) => checkManagementKey(candidate),
  });

  usePageTitle("Sign in");

  if (key !== null) {
    return <Navigate to={to} replace />;
  }

  const onSubmit = form.handleSubmit(({ key: candidate }) => {
    check.mutate(candidate, {
      onSuccess: () => {
        signIn(candidate);
      },
    });
  });

  const problem = check.isError ? signInProblem(check.error) : null;
  const keyError = form.formState.errors.key?.message;

  return (
    <main className="mx-auto flex min-h-screen w-full max-w-md flex-col justify-center gap-5 px-4 py-10">
      <Brand className="self-center" />
      <div className="rounded-lg border border-line bg-surface px-5 py-6 shadow-sm">
        <h1 className="mb-1 text-lg font-semibold">Sign in to the dashboard</h1>
        <p className="mb-5 text-muted">
          Use the management key this server was set up with. It stays in this tab and is
          forgotten when the tab closes.
        </p>
        <div className="space-y-4">
          {signOutReason === "key-rejected" && (
            <Alert tone="warn" title="You were signed out">
              <p>The server stopped accepting the key this tab held. Sign in with the current key.</p>
            </Alert>
          )}
          {signOutReason === "signed-out" && (
            <Alert tone="ok" live>
              <p>You signed out.</p>
            </Alert>
          )}
          {safeMode && (
            <Alert tone="warn" title="The proxy is in safe mode">
              <p>
                Its client API keys are still CLIProxyAPI&apos;s examples, so it refuses proxy
                requests. Sign in to replace them with keys of your own.
              </p>
            </Alert>
          )}
          <form noValidate onSubmit={(event) => void onSubmit(event)} className="space-y-4">
            <TextField
              label="Management key"
              secret
              revealLabel="Show the key"
              autoFocus
              hint={
                <>
                  The <Code>remote-management.secret-key</Code> in config.yaml, or the{" "}
                  <Code>MANAGEMENT_PASSWORD</Code> environment variable. On the computer the
                  server runs on, its local password works too: the one given with{" "}
                  <Code>-password</Code>, or the one the terminal UI&apos;s standalone mode sets.
                </>
              }
              error={keyError}
              {...form.register("key")}
            />
            {problem !== null && <ProblemNotice problem={problem} live />}
            <Button type="submit" variant="primary" className="w-full" disabled={check.isPending}>
              {check.isPending ? <Spinner /> : <KeyRound aria-hidden="true" className="size-4" />}
              {check.isPending ? "Checking the key…" : "Sign in"}
            </Button>
          </form>
        </div>
      </div>
    </main>
  );
}
