// What the server's states and reasons for a credential mean, in plain
// words, and what to do about each. Every such text on the Credentials
// page comes from here: the credential states (upstream's `Status`), the
// cooldown reasons (`CooldownView.reason`), the status messages the
// manager leaves on a failed credential, and why a sign-in failed.

import type { Cooldown, CooldownReason, Credential, SignInProvider } from "../../api/credentials";
import type { BadgeTone } from "../../components/Badge";
import { formatSeconds } from "../../lib/format";

/** A provider's name as people know it. */
export function providerName(provider: string): string {
  switch (provider.trim().toLowerCase()) {
    case "claude":
    case "anthropic":
      return "Claude";
    case "codex":
      return "Codex";
    case "gemini":
      return "Gemini";
    case "gemini-cli":
      return "Gemini CLI";
    case "gemini-interactions":
      return "Gemini Interactions";
    case "vertex":
      return "Vertex AI";
    case "xai":
      return "xAI";
    case "meta":
      return "Meta";
    case "":
      return "Unknown provider";
    default:
      return provider;
  }
}

// ------------------------------------------------------------- reasons

export interface ReasonText {
  /** What happened, in a few words. */
  title: string;
  /** What it means. */
  meaning: string;
  /** What to do about it. */
  action: string;
}

const REASONS: Record<CooldownReason, ReasonText> = {
  credential_quota: {
    title: "The account's quota is used up",
    meaning:
      "The provider says this account has used its whole allowance, so the server rests it for every model until the allowance comes back.",
    action:
      "Wait until then, or add another account to share the load. If you know the allowance is back, reset the cooldown.",
  },
  quota: {
    title: "Rate-limited",
    meaning:
      "The provider answered 429 Too Many Requests: a rate limit or a quota ran out. The server rests the credential, longer each time it happens again.",
    action:
      "Wait, or add another credential to share the load. Reset the cooldown if you know the limit has lifted.",
  },
  cloudflare_challenge: {
    title: "Stopped by a Cloudflare check",
    meaning:
      "The provider's Cloudflare protection answered with a browser check instead of the API, which the proxy can't pass.",
    action:
      "Wait for it to end. If it keeps happening, the server's network address may be flagged: give the credential another proxy-url.",
  },
  model_not_supported: {
    title: "The model isn't available to it",
    meaning:
      "The provider says this account can't use the model, so the server stops sending that model's requests to it.",
    action: "Use another model with this credential, or add one whose plan includes the model.",
  },
  invalid_grant: {
    title: "The sign-in has expired or was revoked",
    meaning:
      "The provider refused the credential's refresh token, so the server can't get new access tokens with it.",
    action:
      "Sign in again with the same account: the new sign-in replaces this one. If it was revoked on purpose, delete it.",
  },
  unauthorized: {
    title: "The provider refused the credential",
    meaning: "The provider answered 401 Unauthorized: the key or token is wrong, expired or revoked.",
    action: "For a sign-in, sign in again. For an API key, check it with the provider and replace it.",
  },
  payment_required: {
    title: "The account needs payment or permission",
    meaning:
      "The provider answered 402 or 403: the account may be out of credit, its plan may have lapsed, or it may not be allowed this request.",
    action: "Check the account's billing and plan with the provider, then reset the cooldown.",
  },
  not_found: {
    title: "The provider couldn't find what was asked for",
    meaning:
      "The provider answered 404 Not Found, often for a model or an endpoint it doesn't have. The server rests the credential for up to 12 hours.",
    action: "Check the model's name and the credential's base URL, then reset the cooldown.",
  },
  transient_error: {
    title: "The provider failed for a moment",
    meaning:
      "The provider timed out or answered with a server error (500 to 504, or one of Cloudflare's 52x), so the server rests the credential briefly.",
    action: "Usually nothing: the server tries it again when the time is up.",
  },
  unknown: {
    title: "Resting after a failure",
    meaning: "The server is resting the credential after a failure it doesn't name.",
    action:
      "The server log around the time it started says more. Reset the cooldown to try it again now.",
  },
};

function isReason(reason: string): reason is CooldownReason {
  return Object.hasOwn(REASONS, reason);
}

/** What a cooldown's reason means; an unknown code reads as "unknown". */
export function explainReason(reason: string): ReasonText {
  return REASONS[isReason(reason) ? reason : "unknown"];
}

/**
 * The reason a failed credential's status message names, as the server
 * writes it (upstream's `cooldownStatusReason`), or null when it is some
 * other text.
 */
export function reasonOfMessage(message: string): CooldownReason | null {
  switch (message.trim()) {
    case "quota":
    case "quota exhausted":
      return "quota";
    case "cloudflare challenge":
      return "cloudflare_challenge";
    case "invalid_grant":
      return "invalid_grant";
    case "unauthorized":
      return "unauthorized";
    case "payment_required":
      return "payment_required";
    case "not_found":
      return "not_found";
    case "model_not_supported":
      return "model_not_supported";
    case "transient upstream error":
      return "transient_error";
    default:
      return null;
  }
}

// -------------------------------------------------------------- health

export interface Health {
  tone: BadgeTone;
  /** One or two words for the badge. */
  label: string;
  /** What state it is in. */
  summary: string;
  /** What to do, when there is something to do. */
  action: string | null;
}

/** The cooldowns on the whole credential, soonest over first. */
export function credentialCooldowns(credential: Credential): Cooldown[] {
  return (credential.cooldowns ?? [])
    .filter((cooldown) => cooldown.scope === "credential")
    .sort((a, b) => a.remaining_seconds - b.remaining_seconds);
}

/** The cooldowns on single models. */
export function modelCooldowns(credential: Credential): Cooldown[] {
  return (credential.cooldowns ?? []).filter((cooldown) => cooldown.scope === "model");
}

/** Whether resetting would change anything: something is resting it. */
export function canReset(credential: Credential): boolean {
  return (
    (credential.cooldowns ?? []).length > 0 ||
    credential.unavailable ||
    credential.next_retry_after !== undefined
  );
}

/** The credential's state, in words, with what to do about it. */
export function credentialHealth(credential: Credential): Health {
  if (credential.disabled || credential.status === "disabled") {
    return {
      tone: "neutral",
      label: "Off",
      summary: "Turned off: the server sends it no requests.",
      action: "Turn it on to use it again.",
    };
  }
  const resting = credentialCooldowns(credential)[0];
  if (resting !== undefined) {
    const reason = explainReason(resting.reason);
    return { tone: "warn", label: "Resting", summary: reason.title, action: reason.action };
  }
  const message = credential.status_message?.trim() ?? "";
  switch (credential.status) {
    case "error": {
      const known = reasonOfMessage(message);
      if (known !== null) {
        const reason = REASONS[known];
        return { tone: "danger", label: "Failing", summary: reason.title, action: reason.action };
      }
      return {
        tone: "danger",
        label: "Failing",
        summary: message === "" ? "Its last request failed." : `Its last request failed: ${message}`,
        action: "The server log says more. Reset it to try it again now.",
      };
    }
    case "refreshing":
      return {
        tone: "info",
        label: "Refreshing",
        summary: "Getting a new access token from the provider.",
        action: null,
      };
    case "pending":
      return {
        tone: "info",
        label: "Waiting",
        summary: "Loaded, and waiting for its first use.",
        action: null,
      };
    case "active":
      if (credential.unavailable) {
        return {
          tone: "warn",
          label: "Resting",
          summary: "The server is resting it after a failure.",
          action: "It is tried again when the time is up. Reset it to try it again now.",
        };
      }
      return {
        tone: "ok",
        label: "Ready",
        summary:
          modelCooldowns(credential).length > 0
            ? "In use, with some models resting: see below."
            : "In use.",
        action: null,
      };
    default:
      return {
        tone: "neutral",
        label: "Unknown",
        summary: "The server hasn't said what state it is in.",
        action: null,
      };
  }
}

// ------------------------------------------------------------ sign-ins

/** Why a sign-in failed, as `get-auth-status` says it, in plain words. */
export function explainSignInError(error: string): ReasonText {
  const message = error.trim();
  if (message === "unknown or expired state") {
    return {
      title: "This sign-in has ended",
      meaning:
        "The server no longer knows it: it was given up, it expired after 30 minutes, or the server restarted.",
      action: "Start the sign-in again.",
    };
  }
  if (message === "Timeout waiting for OAuth callback") {
    return {
      title: "The sign-in waited too long",
      meaning: "The server waits five minutes for the provider to send you back, and that time ran out.",
      action: "Start again, and finish signing in within five minutes.",
    };
  }
  if (message === "Bad request" || message === "Bad Request") {
    return {
      title: "The provider reported an error",
      meaning: "The provider sent you back with an error instead of a sign-in: it may have been declined.",
      action: "Start again, and allow the access the provider asks for.",
    };
  }
  if (message.startsWith("Timeout exchanging authorization code")) {
    return {
      title: "The provider took too long",
      meaning: "The server couldn't finish the sign-in with the provider within a minute.",
      action: "Check that the server reaches the provider, through its proxy-url if it has one, and start again.",
    };
  }
  if (message.startsWith("Failed to exchange authorization code")) {
    return {
      title: "The provider didn't accept the sign-in",
      meaning: `The provider refused the code the sign-in sent back. A code works once and only for a few minutes. It said: ${message}`,
      action: "Start again.",
    };
  }
  if (message.startsWith("Failed to save authentication tokens")) {
    return {
      title: "The server couldn't save the credential",
      meaning:
        "The sign-in worked, but the server couldn't write its file. It also saves none for an account without an email address.",
      action: "Check that the server can write to its auth directory; its log says more.",
    };
  }
  // What `oauth-callback` says of a pasted address it can't use.
  if (
    message === "state is required" ||
    message === "invalid state" ||
    message === "code or error is required" ||
    message === "invalid redirect_url" ||
    message === "State code error"
  ) {
    return {
      title: "That isn't the address this sign-in sent you to",
      meaning: "The server found no sign-in code for this sign-in in it.",
      action:
        "Copy the whole address from the address bar of the page the provider sent you to, and paste it again.",
    };
  }
  if (message === "provider does not match state") {
    return {
      title: "That address is from another provider's sign-in",
      meaning: "It belongs to a sign-in with another provider.",
      action: "Paste the address from this sign-in, or start again.",
    };
  }
  if (message === "oauth flow is already completed") {
    return {
      title: "This sign-in has already finished",
      meaning: "The server already has its result.",
      action: "Close this, and look for the credential in the list.",
    };
  }
  if (message === "oauth flow is not pending") {
    return {
      title: "This sign-in has ended",
      meaning: "It is no longer waiting for an answer: it was given up or has failed.",
      action: "Start the sign-in again.",
    };
  }
  return {
    title: "The sign-in failed",
    meaning: message === "" || message === "Authentication failed" ? "The server didn't say why." : message,
    action: "Start the sign-in again.",
  };
}

/** The port each provider's sign-in comes back to, on the server's computer. */
const CALLBACK_PORTS: Record<SignInProvider, number> = { claude: 54545, codex: 1455 };

/** Why a sign-in couldn't start, when it is one the page can explain. */
export interface StartProblem {
  reason: ReasonText;
  /** Whether starting again without the local callback would work. */
  pasteOnly: boolean;
}

/**
 * Why `anthropic-auth-url` or `codex-auth-url` refused to start, from its
 * status and `error`, or null for a refusal the general notice explains.
 */
export function explainStartError(
  provider: SignInProvider,
  status: number,
  error: string | null,
): StartProblem | null {
  const name = providerName(provider);
  if (error === "failed to start callback server" || error === "callback server unavailable") {
    return {
      pasteOnly: true,
      reason: {
        title: "The server couldn't listen for the sign-in's answer",
        meaning: `When you finish signing in, ${name} sends your browser to port ${String(CALLBACK_PORTS[provider])} on the server's computer, and the server couldn't listen there: another program or sign-in may be using the port.`,
        action:
          "You can still sign in: start without it, and paste the address of the page the provider sends you to.",
      },
    };
  }
  if (status === 429 || error === "too many oauth sessions") {
    return {
      pasteOnly: false,
      reason: {
        title: "Too many sign-ins are open",
        meaning: "The server keeps a limited number of unfinished sign-ins, and that many are open.",
        action: "Wait a few minutes for the old ones to expire, then start again.",
      },
    };
  }
  if (error === "server shutting down") {
    return {
      pasteOnly: false,
      reason: {
        title: "The server is shutting down",
        meaning: "It starts no sign-ins while it stops.",
        action: "Start again once it is running again.",
      },
    };
  }
  return null;
}

/**
 * Whether `pasted` is the address a sign-in with `state` sent the browser
 * to: why not, or null when it is.
 */
export function pastedAddressProblem(pasted: string, state: string): string | null {
  let url: URL;
  try {
    url = new URL(pasted.trim());
  } catch {
    return "That isn't a web address. Copy the whole address, starting with http.";
  }
  const params = url.searchParams;
  if (params.get("state") !== state) {
    return "That address isn't from this sign-in. Paste the address of the page this sign-in sent you to.";
  }
  if ((params.get("code") ?? "") === "" && (params.get("error") ?? "") === "") {
    return "That address has no sign-in code in it. Copy all of it, including everything after the question mark.";
  }
  return null;
}

/** How long a cooldown has left, roughly: "about 4 min", "less than a minute". */
export function timeLeft(seconds: number): string {
  if (seconds < 60) {
    return "less than a minute";
  }
  const minutes = Math.round(seconds / 60);
  return `about ${formatSeconds(minutes * 60)}`;
}
