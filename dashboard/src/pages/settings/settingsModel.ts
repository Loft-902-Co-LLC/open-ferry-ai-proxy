// The settings the Settings form changes, each through its own management
// route (`PATCH /v0/management/<setting>` with `{"value": ...}`), so a save
// touches nothing else in config.yaml. Values are read from
// `GET /v0/management/config` as the server uses them: the config module
// (open-ferry-core's config::normalize) loads a negative log size as 0 and
// a negative error-log count as 10, and takes an unknown routing strategy as
// round-robin.

import { MANAGEMENT } from "../../api/management";
import { wholeNumberField } from "../../lib/fields";
import { z } from "../../lib/zod";

export const STRATEGIES = ["round-robin", "weighted-round-robin", "fill-first"] as const;
export type Strategy = (typeof STRATEGIES)[number];

export const STRATEGY_LABELS: Record<Strategy, string> = {
  "round-robin": "Round robin",
  "weighted-round-robin": "Weighted round robin",
  "fill-first": "Fill first",
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
    default:
      return "round-robin";
  }
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
      return "open-ferry can't use a SOCKS5 proxy yet. Use an HTTP or HTTPS proxy.";
    default:
      return "The address starts with http:// or https://.";
  }
}

/** A proxy address with any user name and password hidden. */
export function redactProxyUrl(value: string): string {
  return value.trim().replace(/^([a-z][a-z0-9+.-]*:\/\/)[^/?#]*@/i, "$1•••@");
}

export const settingsSchema = z.object({
  proxyUrl: z
    .string()
    .trim()
    .superRefine((value, context) => {
      const problem = proxyUrlProblem(value);
      if (problem !== null) {
        context.addIssue({ code: "custom", message: problem });
      }
    }),
  routingStrategy: z.enum(STRATEGIES),
  requestRetry: wholeNumberField("Retries", 0, 1_000),
  maxRetryCredentials: wholeNumberField("Credentials per round", 0, 10_000),
  maxRetryInterval: wholeNumberField("The longest wait", 0, 86_400),
  forceModelPrefix: z.boolean(),
  debug: z.boolean(),
  loggingToFile: z.boolean(),
  logsMaxTotalSizeMb: wholeNumberField("The log directory's limit", 0, 10_000_000),
  requestLog: z.boolean(),
  errorLogsMaxFiles: wholeNumberField("The number of failed-request logs kept", 0, 1_000_000),
  usageStatisticsEnabled: z.boolean(),
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
  /** The management route that changes it. */
  path: string;
}

export const SETTINGS: Record<SettingId, SettingInfo> = {
  proxyUrl: { label: "Proxy", configKey: "proxy-url", path: `${MANAGEMENT}/proxy-url` },
  routingStrategy: {
    label: "How credentials are picked",
    configKey: "routing.strategy",
    path: `${MANAGEMENT}/routing/strategy`,
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
};

export const SETTING_IDS = Object.keys(SETTINGS) as SettingId[];

function fieldOf(object: unknown, key: string): unknown {
  return object !== null && typeof object === "object"
    ? (object as Record<string, unknown>)[key]
    : undefined;
}

function wholeNumber(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? Math.trunc(value) : 0;
}

/** The settings in `config`, an answer of `GET /config`, as the server uses them. */
export function settingValuesOf(config: unknown): SettingValues {
  const flag = (key: string) => fieldOf(config, key) === true;
  const count = (key: string) => Math.max(0, wholeNumber(fieldOf(config, key)));
  const proxy = fieldOf(config, "proxy-url");
  const errorLogsField = fieldOf(config, "error-logs-max-files");
  const errorLogs = typeof errorLogsField === "number" ? wholeNumber(errorLogsField) : -1;
  return {
    proxyUrl: typeof proxy === "string" ? proxy.trim() : "",
    routingStrategy: strategyOf(fieldOf(fieldOf(config, "routing"), "strategy")),
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
  };
}

/** `values` as the form holds them. */
export function formValuesOf(values: SettingValues): SettingsInput {
  return {
    ...values,
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

function plural(count: number, one: string, many: string): string {
  return `${count.toLocaleString("en")} ${count === 1 ? one : many}`;
}

/** `value` of setting `id`, in words. */
export function describeSetting<K extends SettingId>(id: K, value: SettingValues[K]): string {
  if (typeof value === "boolean") {
    return value ? "On" : "Off";
  }
  if (typeof value === "number") {
    switch (id) {
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
  if (id === "routingStrategy") {
    return STRATEGY_LABELS[strategyOf(value)];
  }
  const proxy = value.trim();
  if (proxy === "") {
    return "The environment's proxy, if any";
  }
  if (/^(direct|none)$/i.test(proxy)) {
    return "No proxy";
  }
  return redactProxyUrl(proxy);
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
