// The settings the Settings form changes, each through a route of its own,
// so a save sends nothing else: upstream's settings by `PATCH
// /v0/management/<setting>` with `{"value": ...}`, and open-ferry's own,
// `routing.quota.*` and `management.separate-address`, which have no such
// route, by `PUT /v8/management/config/<path>` with the bare value. Values
// are read from `GET /v0/management/config` as the server uses them: the
// config module (open-ferry-core's config::normalize) loads a negative log
// size as 0 and a negative error-log count as 10, and takes an unknown
// routing strategy as round-robin. That answer has no `management` or
// `server` section, so the management address, and what checking it needs,
// are read from the file through the v8 config route (see ServerFacts).

import type { ApiRequest } from "../../api/client";
import { MANAGEMENT, V8_CONFIG } from "../../api/management";
import { countField, wholeNumberField } from "../../lib/fields";
import { parseGoDuration } from "../../lib/goDuration";
import { z } from "../../lib/zod";
import { managementAddressProblem, parseManagementAddress } from "./managementAddress";

export const STRATEGIES = ["round-robin", "weighted-round-robin", "fill-first", "quota"] as const;
export type Strategy = (typeof STRATEGIES)[number];

export const STRATEGY_LABELS: Record<Strategy, string> = {
  "round-robin": "Round robin",
  "weighted-round-robin": "Weighted round robin",
  "fill-first": "Fill first",
  quota: "By quota",
};

/**
 * The strategy the server uses for `written`, as `Config::routing_strategy`
 * reads it: its canonical or short name in any case, else round-robin.
 */
export function strategyOf(written: unknown): Strategy {
  const name = typeof written === "string" ? written.trim().toLowerCase() : "";
  switch (name) {
    case "weighted-round-robin":
    case "weightedroundrobin":
    case "wrr":
      return "weighted-round-robin";
    case "fill-first":
    case "fillfirst":
    case "ff":
      return "fill-first";
    case "quota":
      return "quota";
    default:
      return "round-robin";
  }
}

/** What routing by quota prefers among the credentials with room (`routing.quota.prefer`). */
export const PREFERENCES = ["soonest-reset", "most-left"] as const;
export type Preference = (typeof PREFERENCES)[number];

export const PREFERENCE_LABELS: Record<Preference, string> = {
  "soonest-reset": "The limit that resets soonest",
  "most-left": "The most quota left",
};

/**
 * What routing by quota prefers for `written`, as `QuotaPrefs::of` reads it
 * (open-ferry-core's manager::quota_rank): `most-left` in any case, with
 * spaces around, else `soonest-reset`.
 */
export function preferenceOf(written: unknown): Preference {
  return typeof written === "string" && written.trim().toLowerCase() === "most-left"
    ? "most-left"
    : "soonest-reset";
}

/**
 * What is wrong with `text`, trimmed, as `routing.quota.check-after`, or
 * null. The server takes empty, zero, a negative time and anything it can't
 * read as a Go duration as off (open-ferry-core's manager::settings); the
 * form takes empty for off, or a time the server reads, zero included.
 */
export function checkAfterProblem(text: string): string | null {
  const value = text.trim();
  if (value === "") {
    return null;
  }
  const nanos = parseGoDuration(value);
  if (nanos === null) {
    return "The server can't read this as a time, and takes it as off. Write a number and a unit with no spaces, such as 1h, 90m or 1h30m (h, m, s, ms, us or ns; a day is 24h).";
  }
  if (nanos < 0n) {
    return "A negative time is off. Leave it empty for off, or write a time such as 1h.";
  }
  return null;
}

/**
 * What is wrong with `value` as the proxy for outbound requests, as the
 * providers' clients read it (open-ferry-providers' `parse_proxy`), or null.
 * Empty uses the environment's proxy; `direct` or `none` uses none.
 */
export function proxyUrlProblem(value: string): string | null {
  const trimmed = value.trim();
  if (trimmed === "" || /^(direct|none)$/i.test(trimmed)) {
    return null;
  }
  let url: URL;
  try {
    url = new URL(trimmed);
  } catch {
    return "Enter an address such as http://proxy.example:8080, or direct, or leave it empty.";
  }
  switch (url.protocol) {
    case "http:":
    case "https:":
      return url.hostname === ""
        ? "The address needs a host, such as http://proxy.example:8080."
        : null;
    case "socks5:":
    case "socks5h:":
      return "The server can't use a SOCKS5 proxy yet. Use an HTTP or HTTPS proxy.";
    default:
      return "The address starts with http:// or https://.";
  }
}

/** A proxy address with any user name and password hidden. */
export function redactProxyUrl(value: string): string {
  return value.trim().replace(/^([a-z][a-z0-9+.-]*:\/\/)[^/?#]*@/i, "$1•••@");
}

/** A text field checked by `problemOf`, which gives null for a good value. */
function checkedText(problemOf: (value: string) => string | null) {
  return z
    .string()
    .trim()
    .superRefine((value, context) => {
      const problem = problemOf(value);
      if (problem !== null) {
        context.addIssue({ code: "custom", message: problem });
      }
    });
}

export const settingsSchema = z.object({
  proxyUrl: checkedText(proxyUrlProblem),
  routingStrategy: z.enum(STRATEGIES),
  quotaPrefer: z.enum(PREFERENCES),
  quotaReservePercent: wholeNumberField("The share kept back", 0, 100),
  quotaCheckAfter: checkedText(checkAfterProblem),
  // The server holds these as 64-bit integers and sets no upper limit.
  requestRetry: countField("Retries"),
  maxRetryCredentials: countField("Credentials per round"),
  maxRetryInterval: countField("The longest wait"),
  forceModelPrefix: z.boolean(),
  debug: z.boolean(),
  loggingToFile: z.boolean(),
  logsMaxTotalSizeMb: countField("The log directory's limit"),
  requestLog: z.boolean(),
  errorLogsMaxFiles: countField("The number of failed-request logs kept"),
  usageStatisticsEnabled: z.boolean(),
  // Its port is checked against the proxy's too, which isn't a value here:
  // see checkSetting.
  managementAddress: checkedText((value) =>
    value === "" ? null : parseManagementAddress(value).problem,
  ),
});

/** The settings as the form holds them: numbers as the text typed. */
export type SettingsInput = z.input<typeof settingsSchema>;
/** The settings as the server takes them. */
export type SettingValues = z.output<typeof settingsSchema>;
export type SettingId = keyof SettingValues;

export interface SettingInfo {
  /** Its name, as the review lists it. */
  label: string;
  /** Its key in config.yaml. */
  configKey: string;
  /**
   * The route that changes it: a v0 route, which takes `{"value": ...}` by
   * PATCH, or with `v8`, a v8 config path, which takes the bare value by PUT.
   */
  path: string;
  v8?: true;
}

export const SETTINGS: Record<SettingId, SettingInfo> = {
  proxyUrl: { label: "Proxy", configKey: "proxy-url", path: `${MANAGEMENT}/proxy-url` },
  routingStrategy: {
    label: "How credentials are picked",
    configKey: "routing.strategy",
    path: `${MANAGEMENT}/routing/strategy`,
  },
  quotaPrefer: {
    label: "Quota preference",
    configKey: "routing.quota.prefer",
    path: `${V8_CONFIG}/routing/quota/prefer`,
    v8: true,
  },
  quotaReservePercent: {
    label: "Quota kept back",
    configKey: "routing.quota.reserve-percent",
    path: `${V8_CONFIG}/routing/quota/reserve-percent`,
    v8: true,
  },
  quotaCheckAfter: {
    label: "Check quota rests after",
    configKey: "routing.quota.check-after",
    path: `${V8_CONFIG}/routing/quota/check-after`,
    v8: true,
  },
  requestRetry: { label: "Retries", configKey: "request-retry", path: `${MANAGEMENT}/request-retry` },
  maxRetryCredentials: {
    label: "Credentials per round",
    configKey: "max-retry-credentials",
    path: `${MANAGEMENT}/max-retry-credentials`,
  },
  maxRetryInterval: {
    label: "Longest wait for a retry",
    configKey: "max-retry-interval",
    path: `${MANAGEMENT}/max-retry-interval`,
  },
  forceModelPrefix: {
    label: "Prefixed credentials need the prefix",
    configKey: "force-model-prefix",
    path: `${MANAGEMENT}/force-model-prefix`,
  },
  debug: { label: "Debug logging", configKey: "debug", path: `${MANAGEMENT}/debug` },
  loggingToFile: {
    label: "Log to files",
    configKey: "logging-to-file",
    path: `${MANAGEMENT}/logging-to-file`,
  },
  logsMaxTotalSizeMb: {
    label: "Log directory limit",
    configKey: "logs-max-total-size-mb",
    path: `${MANAGEMENT}/logs-max-total-size-mb`,
  },
  requestLog: { label: "Request logs", configKey: "request-log", path: `${MANAGEMENT}/request-log` },
  errorLogsMaxFiles: {
    label: "Failed-request logs kept",
    configKey: "error-logs-max-files",
    path: `${MANAGEMENT}/error-logs-max-files`,
  },
  usageStatisticsEnabled: {
    label: "Usage statistics",
    configKey: "usage-statistics-enabled",
    path: `${MANAGEMENT}/usage-statistics-enabled`,
  },
  managementAddress: {
    label: "Management address",
    configKey: "management.separate-address",
    path: `${V8_CONFIG}/management/separate-address`,
    v8: true,
  },
};

export const SETTING_IDS = Object.keys(SETTINGS) as SettingId[];

/** The call that saves `value` as setting `id`. */
export function saveCall<K extends SettingId>(
  id: K,
  value: SettingValues[K],
): { path: string; request: ApiRequest } {
  const { path, v8 } = SETTINGS[id];
  return {
    path,
    request: v8 === true ? { method: "PUT", json: value } : { method: "PATCH", json: { value } },
  };
}

/**
 * What the Settings tab reads from config.yaml through the v8 config route,
 * since `GET /config` doesn't give it: the management address, and what
 * checking it needs.
 */
export interface ServerFacts {
  /** `management.separate-address`, trimmed; empty when unset. */
  separateAddress: string;
  /** `management.allow-remote`. */
  allowRemote: boolean;
  /** `server.port`, the proxy's port; 0 when unset, as the server loads it. */
  proxyPort: number;
}

/** Where each of ServerFacts is in config.yaml's v8 layout. */
export const FACT_PATHS: Record<keyof ServerFacts, readonly string[]> = {
  separateAddress: ["management", "separate-address"],
  allowRemote: ["management", "allow-remote"],
  proxyPort: ["server", "port"],
};

function fieldOf(object: unknown, key: string): unknown {
  return object !== null && typeof object === "object"
    ? (object as Record<string, unknown>)[key]
    : undefined;
}

function wholeNumber(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? Math.trunc(value) : 0;
}

/** ServerFacts from the values at FACT_PATHS, undefined where the file has none. */
export function serverFactsOf(values: Partial<Record<keyof ServerFacts, unknown>>): ServerFacts {
  return {
    separateAddress:
      typeof values.separateAddress === "string" ? values.separateAddress.trim() : "",
    allowRemote: values.allowRemote === true,
    proxyPort: wholeNumber(values.proxyPort),
  };
}

/**
 * ServerFacts, read with `call` from the whole of config.yaml in the v8
 * layout, `GET /v8/management/config`. One read of the whole: the route
 * answers a path the file has nothing at with 404 `not_found`, which a
 * browser logs as an error, and a file without these keys is the usual
 * case. Only the facts are kept, not the rest of the file.
 */
export async function readServerFacts(
  call: (path: string, request?: ApiRequest) => Promise<unknown>,
  signal?: AbortSignal,
): Promise<ServerFacts> {
  const file = await call(V8_CONFIG, { signal });
  const facts = Object.keys(FACT_PATHS) as (keyof ServerFacts)[];
  return serverFactsOf(
    Object.fromEntries(
      facts.map((fact) => [fact, FACT_PATHS[fact].reduce<unknown>(fieldOf, file)]),
    ),
  );
}

/**
 * The settings in `config`, an answer of `GET /config`, as the server uses
 * them, with the management address from `facts` (empty without).
 */
export function settingValuesOf(config: unknown, facts?: ServerFacts): SettingValues {
  const flag = (key: string) => fieldOf(config, key) === true;
  const count = (key: string) => Math.max(0, wholeNumber(fieldOf(config, key)));
  const proxy = fieldOf(config, "proxy-url");
  const errorLogsField = fieldOf(config, "error-logs-max-files");
  const errorLogs = typeof errorLogsField === "number" ? wholeNumber(errorLogsField) : -1;
  const routing = fieldOf(config, "routing");
  const quota = fieldOf(routing, "quota");
  const checkAfter = fieldOf(quota, "check-after");
  return {
    proxyUrl: typeof proxy === "string" ? proxy.trim() : "",
    routingStrategy: strategyOf(fieldOf(routing, "strategy")),
    quotaPrefer: preferenceOf(fieldOf(quota, "prefer")),
    quotaReservePercent: Math.min(100, Math.max(0, wholeNumber(fieldOf(quota, "reserve-percent")))),
    quotaCheckAfter: typeof checkAfter === "string" ? checkAfter.trim() : "",
    requestRetry: count("request-retry"),
    maxRetryCredentials: count("max-retry-credentials"),
    maxRetryInterval: count("max-retry-interval"),
    forceModelPrefix: flag("force-model-prefix"),
    debug: flag("debug"),
    loggingToFile: flag("logging-to-file"),
    logsMaxTotalSizeMb: count("logs-max-total-size-mb"),
    requestLog: flag("request-log"),
    errorLogsMaxFiles: errorLogs < 0 ? 10 : errorLogs,
    usageStatisticsEnabled: flag("usage-statistics-enabled"),
    managementAddress: facts?.separateAddress ?? "",
  };
}

/** `values` as the form holds them. */
export function formValuesOf(values: SettingValues): SettingsInput {
  return {
    ...values,
    quotaReservePercent: String(values.quotaReservePercent),
    requestRetry: String(values.requestRetry),
    maxRetryCredentials: String(values.maxRetryCredentials),
    maxRetryInterval: String(values.maxRetryInterval),
    logsMaxTotalSizeMb: String(values.logsMaxTotalSizeMb),
    errorLogsMaxFiles: String(values.errorLogsMaxFiles),
  };
}

/** One setting's value as the form holds it. */
export function formValueOf(value: SettingValues[SettingId]): SettingsInput[SettingId] {
  return typeof value === "number" ? String(value) : value;
}

/**
 * Whether `input`, the form's value for a setting, differs from `value`, the
 * server's. Text that isn't a number, in a number's place, differs.
 */
export function isEdited(
  input: SettingsInput[SettingId] | undefined,
  value: SettingValues[SettingId],
): boolean {
  if (typeof input !== "string") {
    return input !== value;
  }
  const text = input.trim();
  if (typeof value !== "number") {
    return text !== value;
  }
  return text !== String(value) && (!/^\d+$/.test(text) || Number(text) !== value);
}

function firstMessage(error: { issues: readonly { message: string }[] }): string {
  return error.issues[0]?.message ?? "This value can't be saved.";
}

/**
 * `input` as the value of setting `id`, or what is wrong with it. With
 * `facts`, a management address on the proxy's port is wrong, as the server
 * refuses it.
 */
function checkSetting<K extends SettingId>(
  id: K,
  input: SettingsInput[K] | undefined,
  facts: ServerFacts | undefined,
): { value: SettingValues[K]; problem: null } | { value: null; problem: string } {
  const result = settingsSchema.shape[id].safeParse(input);
  if (!result.success) {
    return { value: null, problem: firstMessage(result.error) };
  }
  const value = result.data as SettingValues[K];
  if (id === "managementAddress" && facts !== undefined && typeof value === "string") {
    const problem = managementAddressProblem(value, facts.proxyPort);
    if (problem !== null) {
      return { value: null, problem };
    }
  }
  return { value, problem: null };
}

/** What is wrong with `input` as the value of setting `id`, or null. */
export function settingProblem(
  id: SettingId,
  input: SettingsInput[SettingId] | undefined,
  facts?: ServerFacts,
): string | null {
  return checkSetting(id, input, facts).problem;
}

export type SettingProblems = Partial<Record<SettingId, string>>;

/**
 * What is wrong with the settings in `loaded`, as the form would show them:
 * values in config.yaml the form wouldn't take if typed in, such as a SOCKS5
 * proxy.
 */
export function loadedProblems(loaded: SettingValues, facts?: ServerFacts): SettingProblems {
  const problems: SettingProblems = {};
  for (const id of SETTING_IDS) {
    const problem = settingProblem(id, formValueOf(loaded[id]), facts);
    if (problem !== null) {
      problems[id] = problem;
    }
  }
  return problems;
}

/**
 * The form's `input` as the server takes it, with only the settings edited
 * from `loaded` checked. A setting left alone keeps its loaded value, and a
 * save doesn't send it, so a value in config.yaml the form wouldn't take
 * never stops the others being saved. Without `loaded`, every setting is
 * checked.
 */
export function readEdited(
  input: SettingsInput,
  loaded: SettingValues | undefined,
  facts?: ServerFacts,
): { values: SettingValues; problems: null } | { values: null; problems: SettingProblems } {
  const values: Partial<SettingValues> = {};
  const problems: SettingProblems = {};
  for (const id of SETTING_IDS) {
    if (loaded !== undefined && !isEdited(input[id], loaded[id])) {
      Object.assign(values, { [id]: loaded[id] });
      continue;
    }
    const checked = checkSetting(id, input[id], facts);
    if (checked.problem === null) {
      Object.assign(values, { [id]: checked.value });
    } else {
      problems[id] = checked.problem;
    }
  }
  return Object.keys(problems).length > 0
    ? { values: null, problems }
    : { values: values as SettingValues, problems: null };
}

function plural(count: number, one: string, many: string): string {
  return `${count.toLocaleString("en")} ${count === 1 ? one : many}`;
}

/** A `routing.quota.check-after`, in words: the time, or off and why. */
function describeCheckAfter(text: string): string {
  if (text === "") {
    return "Off";
  }
  const nanos = parseGoDuration(text);
  if (nanos === null) {
    return `Off (${text} isn't a time)`;
  }
  return nanos > 0n ? text : `Off (${text})`;
}

/** `value` of setting `id`, in words. */
export function describeSetting<K extends SettingId>(id: K, value: SettingValues[K]): string {
  if (typeof value === "boolean") {
    return value ? "On" : "Off";
  }
  if (typeof value === "number") {
    switch (id) {
      case "quotaReservePercent":
        return value === 0 ? "None" : `${String(value)}%`;
      case "requestRetry":
        return value === 0 ? "None" : plural(value, "retry", "retries");
      case "maxRetryCredentials":
        return value === 0 ? "All of them" : plural(value, "credential", "credentials");
      case "maxRetryInterval":
        return value === 0 ? "Doesn't wait" : plural(value, "second", "seconds");
      case "logsMaxTotalSizeMb":
        return value === 0 ? "No limit" : `${value.toLocaleString("en")} MB`;
      case "errorLogsMaxFiles":
        return value === 0 ? "All of them" : plural(value, "file", "files");
      default:
        return value.toLocaleString("en");
    }
  }
  const text = value.trim();
  switch (id) {
    case "routingStrategy":
      return STRATEGY_LABELS[strategyOf(text)];
    case "quotaPrefer":
      return PREFERENCE_LABELS[preferenceOf(text)];
    case "quotaCheckAfter":
      return describeCheckAfter(text);
    case "managementAddress":
      return text === "" ? "None: on the proxy's port" : text;
    default:
      break;
  }
  if (text === "") {
    return "The environment's proxy, if any";
  }
  if (/^(direct|none)$/i.test(text)) {
    return "No proxy";
  }
  return redactProxyUrl(text);
}

/** A setting a save changes. */
export interface SettingChange<K extends SettingId = SettingId> {
  id: K;
  /** Its value on the server now. */
  now: SettingValues[K];
  after: SettingValues[K];
  /** It changed on the server since the page loaded it. */
  movedOnServer: boolean;
}

/**
 * The settings to save: those the user changed from what the page loaded
 * (`loaded`), which the server (`fresh`, read just now) doesn't already
 * have. Settings the user left alone aren't sent, even where the server's
 * value has moved on.
 */
export function settingChanges(
  loaded: SettingValues,
  edited: SettingValues,
  fresh: SettingValues,
): SettingChange[] {
  return SETTING_IDS.filter((id) => edited[id] !== loaded[id] && edited[id] !== fresh[id]).map(
    (id) => ({ id, now: fresh[id], after: edited[id], movedOnServer: fresh[id] !== loaded[id] }),
  );
}
