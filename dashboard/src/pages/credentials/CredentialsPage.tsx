import { LogIn } from "lucide-react";
import { useSearchParams } from "react-router";

import { SIGN_IN_PROVIDERS, type SignInProvider } from "../../api/credentials";
import { Button } from "../../components/Button";
import { PageHeader } from "../../components/PageHeader";
import { ClaudeCliEntries } from "./ClaudeCliEntries";
import { CredentialList } from "./CredentialList";
import { ProviderKeys } from "./ProviderKeys";
import { SignInDialog } from "./SignInDialog";
import { signInName } from "./credentialStates";

/** What `?start=` opens: a sign-in, or the add-a-key dialog. */
export type StartParam = SignInProvider | "key";

function signInOf(start: string | null): SignInProvider | null {
  return SIGN_IN_PROVIDERS.find((provider) => provider === start) ?? null;
}

/**
 * The credentials the server sends requests with: its sign-ins and
 * credential files, and from config.yaml its Claude Code accounts
 * (`claude-cli`) and provider API keys. `?start=`
 * opens a sign-in (`claude`, `codex`) or the add-a-key dialog (`key`), so
 * other pages can link straight to them; opening one starts nothing.
 */
export function CredentialsPage() {
  const [params, setParams] = useSearchParams();
  const start = params.get("start");
  const signIn = signInOf(start);

  const open = (value: StartParam | null) => {
    setParams(
      (current) => {
        const next = new URLSearchParams(current);
        if (value === null) {
          next.delete("start");
        } else {
          next.set("start", value);
        }
        return next;
      },
      { replace: true },
    );
  };

  return (
    <>
      <PageHeader
        title="Credentials"
        description="The accounts and keys the server sends requests to providers with."
        actions={SIGN_IN_PROVIDERS.map((provider) => (
          <Button
            key={provider}
            onClick={() => {
              open(provider);
            }}
          >
            <LogIn aria-hidden="true" className="size-4" />
            Sign in with {signInName(provider)}
          </Button>
        ))}
      />
      <div className="space-y-6">
        <CredentialList />
        <ClaudeCliEntries />
        <ProviderKeys
          adding={start === "key"}
          onAdd={() => {
            open("key");
          }}
          onAddClosed={() => {
            open(null);
          }}
        />
      </div>
      {signIn !== null && (
        <SignInDialog
          key={signIn}
          provider={signIn}
          onClose={() => {
            open(null);
          }}
        />
      )}
    </>
  );
}
