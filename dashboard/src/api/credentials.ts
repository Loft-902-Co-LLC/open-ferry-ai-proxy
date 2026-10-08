// The credentials the proxy sends requests with, through CLIProxyAPI's
// management API, with the shapes upstream v8.0.20 answers with:
// credential files and sign-ins (`auth-files`), the Claude and Codex
// sign-ins (OAuth), and the provider API keys in config.yaml.

import { MANAGEMENT } from "./management";

// ------------------------------------------------------------ credentials

/** `GET`: the credentials; `POST`: upload files; `DELETE ?name=`: delete one. */
export const AUTH_FILES = `${MANAGEMENT}/auth-files`;
/** `PATCH {"name", "auth_index"?, "disabled"}`: turn one off or on. */
export const AUTH_FILE_STATUS = `${MANAGEMENT}/auth-files/status`;
/** `POST {"auth_index"}`: clear one credential's cooldowns. */
export const RESET_COOLDOWN = `${MANAGEMENT}/reset-quota`;
/** `POST {"auth_index"}`: ask the provider for one credential's quota. */
export const QUOTA_FETCH = `${MANAGEMENT}/quota/fetch`;

/** What the server makes of a credential. */
export type CredentialStatus = "unknown" | "active" | "pending" | "refreshing" | "error" | "disabled";

/** Why a cooldown runs: fixed codes, never a provider's own message. */
export type CooldownReason =
  | "credential_quota"
  | "quota"
  | "cloudflare_challenge"
  | "model_not_supported"
  | "invalid_grant"
  | "unauthorized"
  | "payment_required"
  | "not_found"
  | "transient_error"
  | "unknown";

/** One cooldown: the server rests the credential, or one of its models. */
export interface Cooldown {
  scope: "credential" | "model";
  /** The model, for a model's cooldown. */
  model_key?: string;
  reason: CooldownReason | (string & {});
  /** When it ends. */
  retry_at: string;
  /** Whole seconds left when the list was made. */
  remaining_seconds: number;
  backoff_level?: number;
  /** The status of the failure behind it, when there was one. */
  http_status?: number;
}

/**
 * Requests in one ten-minute window. `time` names it by the server's clock,
 * in the server's time zone ("15:00-15:10"), so the dashboard goes by the
 * window's place in the list instead (see `lastUsed`).
 */
export interface RecentRequests {
  time: string;
  success: number;
  failed: number;
}

/** One credential, as `GET auth-files` lists it. */
export interface Credential {
  id: string;
  auth_index: string;
  /** Its file's name, or its ID when it has none. */
  name: string;
  /** The provider: claude, codex, gemini, vertex, an OpenAI-compatible one… */
  provider: string;
  type?: string;
  label?: string;
  status: CredentialStatus | (string & {});
  status_message?: string;
  disabled: boolean;
  unavailable: boolean;
  runtime_only?: boolean;
  /** "file" when it has a file in the auth directory; else "memory". */
  source: "file" | "memory" | (string & {});
  size?: number;
  success: number;
  failed: number;
  recent_requests?: RecentRequests[];
  supports_quota?: boolean;
  email?: string;
  project_id?: string;
  /** "oauth" for a sign-in, "api_key" for a key. */
  account_type?: string;
  /** The account: an email, or for an API key the key itself. */
  account?: string;
  created_at?: string;
  modtime?: string;
  updated_at?: string;
  last_refresh?: string;
  next_retry_after?: string;
  path?: string;
  /** What a Codex sign-in's ID token says about the plan. */
  id_token?: { plan_type?: string; chatgpt_account_id?: string } & Record<string, unknown>;
  priority?: number;
  note?: string;
  cooldowns?: Cooldown[] | null;
  /**
   * The quota headers of the last Claude or Codex response that had any;
   * without `observed_at` for none, or for another provider.
   */
  quota?: QuotaObservation;
  /** The same, by model. */
  model_quotas?: Record<string, QuotaObservation>;
}

/** What a provider's response said of the account's quota. */
export interface QuotaObservation {
  /** When the response came. */
  observed_at?: string;
  /** Its quota headers, by name, as the provider sent them. */
  signals?: Record<string, string>;
}

/** `GET auth-files`. */
export interface CredentialList {
  files: Credential[] | null;
  observed_at?: string;
}

/** `PATCH auth-files/status`. */
export interface StatusAnswer {
  status: "ok";
  disabled: boolean;
}

/**
 * `POST auth-files`. One file answers `{"status":"ok"}`, or its error;
 * several answer the names written, with a 207 and `partial` when some
 * failed.
 */
export interface UploadAnswer {
  status: "ok" | "partial";
  /** How many were written. */
  uploaded?: number;
  /** The names written. */
  files?: string[];
  failed?: { name: string; error: string }[];
}

/** `POST reset-quota`. */
export interface ResetAnswer {
  status: "ok";
  auth_index: string;
  /** The models put back in rotation. */
  models?: string[] | null;
}

/** One quota window or limit. */
export interface QuotaBucket {
  window?: string;
  /** 0 to 1. */
  remainingFraction: number;
  resetTime?: string;
  description?: string;
}

/** `POST quota/fetch`. */
export interface QuotaAnswer {
  subscription?: { plan?: string; tierName?: string; tierId?: string };
  summary?: {
    key: string;
    label: string;
    value: number;
    unit?: string;
    format?: string;
    currency?: string;
  }[];
  groups?: { displayName?: string; buckets?: QuotaBucket[] }[];
  serverTimeOffsetMs?: number;
}

// ---------------------------------------------------------------- sign-ins

export type SignInProvider = "claude" | "codex";

export const SIGN_IN_PROVIDERS: readonly SignInProvider[] = ["claude", "codex"];

/** `GET ?is_webui=true`: starts a sign-in; answers `{"url", "state"}`. */
export const SIGN_IN_START: Record<SignInProvider, string> = {
  claude: `${MANAGEMENT}/anthropic-auth-url`,
  codex: `${MANAGEMENT}/codex-auth-url`,
};
/** `GET ?state=`: `wait`, `ok`, or `error` with why. */
export const SIGN_IN_STATUS = `${MANAGEMENT}/get-auth-status`;
/** `POST {"provider", "redirect_url"}`: finishes a sign-in by hand. */
export const SIGN_IN_CALLBACK = `${MANAGEMENT}/oauth-callback`;
/** `DELETE ?state=`: gives a sign-in up. */
export const SIGN_IN_SESSION = `${MANAGEMENT}/oauth-session`;

/** `GET anthropic-auth-url` and `codex-auth-url`. */
export interface SignInStart {
  status: "ok";
  url: string;
  state: string;
}

/** `GET get-auth-status`. */
export interface SignInStatus {
  status: "wait" | "ok" | "error";
  error?: string;
}

// -------------------------------------------------- provider API keys

/** The provider key lists the dashboard adds to, in config.yaml. */
export type KeyProvider = "claude" | "codex" | "gemini";

export const KEY_PROVIDERS: readonly KeyProvider[] = ["claude", "codex", "gemini"];

/**
 * Each list's route and its key in config.yaml and in the answer: `GET`
 * gives `{"<list>": [...]}`, `PUT` replaces the list, `DELETE ?index=`
 * removes one entry.
 */
export const KEY_LISTS: Record<KeyProvider, { list: string; path: string }> = {
  claude: { list: "claude-api-key", path: `${MANAGEMENT}/claude-api-key` },
  codex: { list: "codex-api-key", path: `${MANAGEMENT}/codex-api-key` },
  gemini: { list: "gemini-api-key", path: `${MANAGEMENT}/gemini-api-key` },
};

/**
 * One entry of a provider key list. The fields the dashboard reads; any
 * others are kept as they came, so a `PUT` of the list keeps them.
 */
export interface ProviderKey {
  "api-key": string;
  "base-url"?: string;
  "proxy-url"?: string;
  prefix?: string;
  /** The index of the credential it makes; read-only. */
  "auth-index"?: string;
  [field: string]: unknown;
}

/** The entries of a `GET` answer of `list`. */
export function keysOf(answer: unknown, list: string): ProviderKey[] {
  if (answer === null || typeof answer !== "object") {
    return [];
  }
  const entries = (answer as Record<string, unknown>)[list];
  return Array.isArray(entries) ? (entries as ProviderKey[]) : [];
}

/** An entry as a `PUT` takes it: without the read-only `auth-index`. */
export function writableKey(entry: ProviderKey): ProviderKey {
  const copy = { ...entry };
  delete copy["auth-index"];
  return copy;
}
